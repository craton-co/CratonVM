// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// =============================================================================
// T2.8: MethodHandle completeness — unreflect, permuteArguments, guardWithTest
// =============================================================================

pub fn register_t28_method_handle_completeness(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // I2 follow-up: register `ClassLoader.{getDefinedPackage,
    // getDefinedPackages, getNamedPackage, definePackage}` overrides so
    // ByteBuddy + Mockito can complete `JavaDispatcher.<clinit>` without
    // tripping a NullPointerException on the never-initialized private
    // `packages` field of user-instantiated ClassLoader subclasses.
    // Piggy-backing here keeps the registration on the real-JDK boot path
    // (this function is called from both the synthetic-jdk and real-JDK
    // branches of `vm_init`) without touching forbidden surface.
    crate::lang_class::i2_register_classloader_package_natives(r);

    // --- T2.8.4: Lookup.unreflect(Method) ---
    let lk = "java/lang/invoke/MethodHandles$Lookup";
    r.register(
        lk,
        "unreflect",
        "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect,
    );

    // Also register unreflectSpecial and unreflectGetter/unreflectSetter
    r.register(
        lk,
        "unreflectSpecial",
        "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_special,
    );
    r.register(
        lk,
        "unreflectGetter",
        "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_getter,
    );
    r.register(
        lk,
        "unreflectSetter",
        "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_setter,
    );
    // `unreflectVarHandle(Field)` is `findVarHandle`'s reflection-shaped twin.
    // Without it the real JDK body ran and produced a VarHandle with no
    // side-table meta, which every `varhandle_get`/`_set` native then failed
    // to recognise.
    r.register(
        lk,
        "unreflectVarHandle",
        "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/VarHandle;",
        lookup_unreflect_var_handle,
    );
    // Real signature is `unreflectConstructor(Constructor)` — the previous
    // `(Class, MethodType)` descriptor never matched the real call, so it fell
    // through to real-JDK bytecode that built a real DirectMethodHandle$Constructor
    // CratonVM's synthetic invoke native can't dispatch (→ invoke returned null;
    // Jackson 3 record/POJO deserialization silently produced null).
    r.register(
        lk,
        "unreflectConstructor",
        "(Ljava/lang/reflect/Constructor;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_constructor,
    );

    // --- T2.8.11: MethodHandles.permuteArguments ---
    let mhs = "java/lang/invoke/MethodHandles";
    r.register(
        mhs,
        "permuteArguments",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;[I)Ljava/lang/invoke/MethodHandle;",
        mhs_permute_arguments,
    );

    // --- T2.8.12: MethodHandles.guardWithTest ---
    r.register(
        mhs,
        "guardWithTest",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        mhs_guard_with_test,
    );

    // --- C26: MethodHandles.dropArgumentsTrusted ---
    // JDK bytecode reads `mh.form.editor()` and NPEs on our synthetic MH
    // whose form field is null. Build a DROP-kind wrapper whose MH_BOUND
    // holds the original MH and whose widened MH_DESC matches the widened
    // signature (so invokeExact arity checks pass); dispatch unwraps and
    // forwards trimmed args to the inner MH.
    r.register(
        mhs,
        "dropArgumentsTrusted",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        make_drop_arguments_adapter,
    );

    // --- Additional MethodHandles combinators ---
    r.register(
        mhs,
        "filterArguments",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if mh_combinator_checks_in_this_mode(&*ctx) {
                if let Some(refusal) = filter_arguments_refusal(ctx, args) {
                    return Err(refusal);
                }
            }
            // filterArguments(target, pos, filters[]): apply each non-null filter
            // to the argument at `pos + i` before dispatching `target`. Used by
            // Groovy's TypeTransformers to coerce a Closure into a SAM/number/
            // array argument — a passthrough leaks the raw Closure to a callee
            // expecting e.g. a Gradle Action.
            let target = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let pos = match args.get(1) {
                Some(Value::Int(p)) => *p,
                _ => 0,
            };
            let filters = match args.get(2) {
                Some(Value::Object(Some(f))) => *f,
                // No filters → behaves like the identity wrapper over target.
                _ => return Ok(Some(Value::Object(Some(target)))),
            };
            // The adapter keeps the target's arity and return type, but each
            // filtered parameter takes its FILTER's parameter type — see
            // `filter_args_adapter_descriptor`. Falls back to the target's
            // descriptor when any piece is unreadable.
            //
            // gc-common w20-f: all of this is read BEFORE the carrier's
            // allocation, and the carrier holds `target` / `filters` at their
            // current addresses; both used to be read and stored after it.
            let target_desc = mh_type_descriptor(ctx, target)
                .or_else(|| mh_read_desc(ctx, target))
                .unwrap_or_default();
            let n = ctx.array_length(filters);
            let mut filter_descs: Vec<Option<String>> = Vec::with_capacity(n);
            let mut readable = true;
            for i in 0..n {
                match ctx.get_array_element(filters, i) {
                    Value::Object(Some(f)) => {
                        let fd = mh_type_descriptor(ctx, f).or_else(|| mh_read_desc(ctx, f));
                        readable &= fd.is_some();
                        filter_descs.push(fd);
                    }
                    _ => filter_descs.push(None),
                }
            }
            let desc = if readable {
                filter_args_adapter_descriptor(&target_desc, &filter_descs, pos)
                    .unwrap_or(target_desc)
            } else {
                target_desc
            };
            let wrapper = alloc_mh_carrier_holding(
                ctx,
                "__mh_filter_wrapper__",
                &[Value::Object(Some(target)), Value::Object(Some(filters)), Value::Int(pos)],
            );
            // gen r4w3/rooting: `wrapper` is stored after
            // `alloc_method_handle` (a GC point); pin and re-read.
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter: ObjectRef =
                alloc_method_handle(ctx, "__adapter__", "filter", &desc, MH_KIND_FILTER)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            ctx.unpin_native_roots(wrapper_pin);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            Ok(Some(Value::Object(Some(adapter))))
        },
    );
    r.register(
        mhs,
        "filterReturnValue",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if mh_combinator_checks_in_this_mode(&*ctx) {
                if let Some(refusal) = filter_return_value_refusal(ctx, args) {
                    return Err(refusal);
                }
            }
            // filterReturnValue(target, filter): invoke target, then pass its
            // result through the unary filter, returning the filter's result.
            // See MH_KIND_RETURN_FILTER's doc comment for why this can no
            // longer be the earlier "return target unchanged" simplification
            // (JRuby's ivar-getter call sites rely on this filter step to
            // substitute the runtime `nil` singleton for a raw Java `null`).
            let target = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let filter = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                // No filter -> behaves like the identity wrapper over target.
                _ => return Ok(Some(Value::Object(Some(target)))),
            };
            // The adapter's parameter types match the target's; its return
            // type matches the filter's return type (JDK contract: the
            // filter's sole parameter type must equal the target's return
            // type, and the filter's own return type becomes the adapter's).
            //
            // gc-common w20-f: read BEFORE the carrier's allocation, and the
            // carrier holds both handles at their current addresses (both
            // used to be read and stored after it).
            let target_desc = mh_type_descriptor(ctx, target)
                .or_else(|| mh_read_desc(ctx, target))
                .unwrap_or_default();
            let filter_desc = mh_type_descriptor(ctx, filter).or_else(|| mh_read_desc(ctx, filter));
            let desc = match (filter_desc, desc_close_paren(&target_desc)) {
                (Some(fd), Some(paren)) => {
                    format!("{}){}", &target_desc[..paren], return_type_desc(&fd))
                }
                _ => target_desc,
            };
            let wrapper = alloc_mh_carrier_holding(
                ctx,
                "__mh_retfilter_wrapper__",
                &[Value::Object(Some(target)), Value::Object(Some(filter))],
            );
            // gen r4w3/rooting: `wrapper` is stored after
            // `alloc_method_handle` (a GC point); pin and re-read.
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter: ObjectRef =
                alloc_method_handle(ctx, "__adapter__", "retfilter", &desc, MH_KIND_RETURN_FILTER)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            ctx.unpin_native_roots(wrapper_pin);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            Ok(Some(Value::Object(Some(adapter))))
        },
    );
    // foldArguments(target, combiner): fold at position 0.
    r.register(
        mhs,
        "foldArguments",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if mh_combinator_checks_in_this_mode(&*ctx) {
                if let Some(refusal) =
                    fold_arguments_refusal(ctx, args.first().copied(), 0, args.get(1).copied())
                {
                    return Err(refusal);
                }
            }
            make_fold_adapter(ctx, args.first().copied(), 0, args.get(1).copied())
        },
    );
    // foldArguments(target, pos, combiner): fold at position `pos`.
    r.register(
        mhs,
        "foldArguments",
        "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let pos = match args.get(1) {
                Some(Value::Int(p)) => *p,
                _ => 0,
            };
            if mh_combinator_checks_in_this_mode(&*ctx) {
                if let Some(refusal) =
                    fold_arguments_refusal(ctx, args.first().copied(), pos, args.get(2).copied())
                {
                    return Err(refusal);
                }
            }
            make_fold_adapter(ctx, args.first().copied(), pos, args.get(2).copied())
        },
    );
    r.register(
        mhs,
        "collectArguments",
        "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let pos = match args.get(1) {
                Some(Value::Int(p)) => *p,
                _ => 0,
            };
            if mh_combinator_checks_in_this_mode(&*ctx) {
                if let Some(refusal) =
                    collect_arguments_refusal(ctx, args.first().copied(), pos, args.get(2).copied())
                {
                    return Err(refusal);
                }
            }
            make_collect_args_adapter(ctx, args.first().copied(), pos, args.get(2).copied())
        },
    );
    r.register(
        mhs,
        "catchException",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            if mh_combinator_checks_in_this_mode(&*ctx) {
                if let Some(refusal) = catch_exception_refusal(ctx, args) {
                    return Err(refusal);
                }
            }
            let target = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let catch_type = args.get(1).copied().unwrap_or(Value::Object(None));
            let handler = args.get(2).copied().unwrap_or(Value::Object(None));
            if let Some(refusal) = mh_catch_exception_type_refusal(ctx, catch_type, handler) {
                return Err(refusal);
            }
            // gc-common w20-f: the descriptor is read and `target` pinned
            // BEFORE the carrier's allocation, which holds the three values at
            // their current addresses (they were stored, and `target` pinned,
            // after it).
            let desc = mh_type_descriptor(ctx, target)
                .or_else(|| mh_read_desc(ctx, target))
                .unwrap_or_default();
            let target_pin = ctx.pin_native_root(target);
            let wrapper = alloc_mh_carrier_holding(
                ctx,
                "__mh_catch_wrapper__",
                &[Value::Object(Some(target)), catch_type, handler],
            );
            // gen r4w3/rooting: `wrapper` and `target` are read after
            // `alloc_method_handle` (a GC point); pin and re-read.
            let wrapper_pin = ctx.pin_native_root(wrapper);
            let adapter: ObjectRef =
                alloc_method_handle(ctx, "__adapter__", "catch", &desc, MH_KIND_CATCH)?;
            let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
            let target = ctx.read_native_pin(target_pin, target);
            ctx.unpin_native_roots(target_pin);
            ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
            if let Value::Object(Some(mt)) = ctx.get_field_by_name(target, "type") {
                ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
            }
            Ok(Some(Value::Object(Some(adapter))))
        },
    );
    // `exactInvoker`/`invoker`/`spreadInvoker` all return a live
    // MH_KIND_INVOKER handle now (see that constant's doc comment for why
    // this matters: Apache Groovy's `IndyInterface.CACHED_INVOKER` is an
    // `exactInvoker` handle that every generic Groovy indy call site
    // dispatches through). Previously these returned an inert stub with no
    // `MH_KIND` set, so `mh_dispatch` silently no-opped on invocation —
    // e.g. every Groovy `beans { ... }`-DSL closure body (Spring's
    // `GroovyBeanDefinitionReader`) never ran, registering zero beans with
    // no exception.
    r.register(
        mhs,
        "exactInvoker",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // C19: propagate the caller's MethodType into the real-JDK
            // `type` field so `mh.type()` / `parameterSlotCount` do not NPE.
            let mt = match args.first() {
                Some(Value::Object(Some(mt))) => Some(*mt),
                _ => None,
            };
            mh_alloc_invoker(ctx, mt, None, true)
        },
    );
    r.register(
        mhs,
        "invoker",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let mt = match args.first() {
                Some(Value::Object(Some(mt))) => Some(*mt),
                _ => None,
            };
            mh_alloc_invoker(ctx, mt, None, false)
        },
    );
    r.register(
        mhs,
        "spreadInvoker",
        "(Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let mt = match args.first() {
                Some(Value::Object(Some(mt))) => Some(*mt),
                _ => None,
            };
            // Spread count N: the trailing N leading-arguments (after the
            // target handle) are supplied packed into a single Object[]
            // instead of flat. Encode N (decimal) in MH_NAME — unused by
            // MH_KIND_INVOKER otherwise — mirroring MH_KIND_DROP's reuse of
            // MH_CLASS for its position encoding.
            let spread = match args.get(1) {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            };
            mh_alloc_invoker(ctx, mt, spread, false)
        },
    );
    // `varHandleInvoker` / `varHandleExactInvoker` (wave 50, lane L4): the
    // JDK's bodies bind a `LambdaForm` invoker; its species links since wave
    // 51, but the handle it returns is not invokable by this model (measured,
    // see `mhs_var_handle_invoker`). A bridge (the category of this
    // registrar), stated at the site: it goes when real `LambdaForm` handles
    // run here
    // (`docs/feature-designs/native-dispatch-architecture.md`,
    // the `java.lang.invoke` rows).
    r.register_with_kind(
        mhs,
        "varHandleInvoker",
        "(Ljava/lang/invoke/VarHandle$AccessMode;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| mhs_var_handle_invoker(ctx, args, false),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        mhs,
        "varHandleExactInvoker",
        "(Ljava/lang/invoke/VarHandle$AccessMode;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| mhs_var_handle_invoker(ctx, args, true),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// T2.8.4 — Lookup.unreflect(Method) -> MethodHandle
// Method layout: 0=Class(decl), 1=String(name), 2=Class(ret), 3=Class[](params),
//   4=Int(modifiers), 5=String(descriptor), 6=Int(paramCount), 7=Int(accessible)
// ---------------------------------------------------------------------------

/// `Lookup.unreflect(null)` / `unreflectSpecial(null, ..)`: the JDK body's first
/// statement is `m.getDeclaringClass()`, so the answer is a NullPointerException
/// with the helpful message below (measured on OpenJDK 25.0.4). This used to be
/// `VmError::Internal`, which is not catchable Java and took the VM down.
pub(crate) fn lk_null_method_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(
            "Cannot invoke \"java.lang.reflect.Method.getDeclaringClass()\" because \"m\" is null"
                .to_string(),
        ),
    }
    .into()
}

pub(crate) fn lookup_unreflect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FIRST statement on purpose — see `lk_enforce_unreflect_access`.
    lk_enforce_unreflect_access(ctx, args, LkUnreflectKind::AccessibleWaives)?;
    // args[0] = this (Lookup), args[1] = Method object
    let method_obj = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(lk_null_method_npe());
        }
    };
    if let Some(result) = lk_lambda_proxy_unreflect(ctx, method_obj) {
        return result;
    }

    // C6: Method uses real-JDK field layout. Read JDK fields by name;
    // descriptor lives in our CratonVM extra slot.
    let class_name = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    // Round 16 wave 17 (mhapp): the declaring class, whose loader resolves
    // the handle's `type`.
    let refc_near = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(class_mirror)) => ctx.class_id_from_mirror(class_mirror),
        _ => None,
    };
    let method_name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let descriptor = crate::lang_class::read_method_descriptor(ctx, method_obj).unwrap_or_default();
    let modifiers = match ctx.get_field_by_name(method_obj, "modifiers") {
        Value::Int(m) => m,
        _ => 0,
    };
    let is_static = (modifiers & 0x0008) != 0;
    let kind = if is_static {
        MH_KIND_STATIC
    } else {
        MH_KIND_VIRTUAL
    };

    let owner = if is_static {
        {
            let m = member_clazz_mirror(ctx, method_obj);
            mh_static_owner_of_mirror(&*ctx, m)
        }
    } else {
        None
    };
    // A protected instance method of another package: the receiver is the
    // lookup class, unless `m.isAccessible()` hands the lookup to the
    // TRUSTED `IMPL_LOOKUP` (JDK 25 `unreflect`; wave 44,
    // `L4W44ProtectedReceiver` row `unreflect`). Read before the load below,
    // a GC point for `method_obj`.
    let restricted = if is_static {
        None
    } else {
        lk_unreflect_protected_receiver(
            &*ctx,
            args,
            method_obj,
            LkMember::Method(&method_name, &descriptor),
        )
    };
    // LOAD only: a static target initializes its class at the first call
    // (`mh_dispatch`), an instance one needs none (see `lookup_load_class`).
    let _ = lookup_load_class(ctx, &class_name);
    let mh = alloc_method_handle_with_receiver(
        ctx,
        &class_name,
        &method_name,
        &descriptor,
        kind,
        restricted.as_deref().filter(|_| kind == MH_KIND_VIRTUAL),
        refc_near,
    )?;
    mh_record_static_owner(ctx, mh, owner);
    Ok(Some(Value::Object(Some(mh))))
}

/// [`lk_protected_receiver`] for an `unreflect*` of `member` (a `Method` or a
/// `Field`): `refc` is the member's declaring class, and a member whose
/// `override` flag is set is looked up by the TRUSTED lookup, which restricts
/// nothing.
pub(crate) fn lk_unreflect_protected_receiver(
    ctx: &dyn NativeContext,
    args: &[Value],
    member_obj: ObjectRef,
    member: LkMember<'_>,
) -> Option<String> {
    if crate::lang_class::accessible_override_is_set(ctx, member_obj) {
        return None;
    }
    let refc = match ctx.get_field_by_name(member_obj, "clazz") {
        Value::Object(Some(m)) => ctx.class_id_from_mirror(m),
        _ => None,
    };
    lk_protected_receiver(ctx, lk_from_of(ctx, args), refc, member)
}

pub(crate) fn lookup_unreflect_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FIRST statement on purpose — see `lk_enforce_unreflect_access`. This one
    // needs PRIVATE and does NOT honour `m.isAccessible()`; the JDK body says
    // so in as many words.
    lk_enforce_unreflect_access(ctx, args, LkUnreflectKind::Special)?;
    // args[0] = Lookup, args[1] = Method, args[2] = specialCaller class
    let method_obj = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(lk_null_method_npe());
        }
    };
    if let Some(result) = lk_lambda_proxy_unreflect(ctx, method_obj) {
        return result;
    }

    let (class_name, refc_id) = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(class_mirror)) => (
            mirror_class_name(ctx, class_mirror).unwrap_or_default(),
            ctx.class_id_from_mirror(class_mirror),
        ),
        _ => (String::new(), None),
    };
    let method_name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let descriptor = crate::lang_class::read_method_descriptor(ctx, method_obj).unwrap_or_default();
    // The handle's receiver type is `specialCaller`, as for `findSpecial`.
    // Read before `lookup_load_class` (a GC point): `args[2]` is not pinned.
    let (caller_name, caller_id) = match args.get(2) {
        Some(Value::Object(Some(caller_obj))) => (
            resolve_class_name_robust(ctx, *caller_obj),
            ctx.class_id_from_mirror(*caller_obj),
        ),
        _ => (None, None),
    };

    // LOAD only (see `lookup_load_class`).
    let _ = lookup_load_class(ctx, &class_name);
    let dispatch_class = special_dispatch_class(
        &*ctx,
        &class_name,
        refc_id,
        caller_id,
        &method_name,
        &descriptor,
    );
    // As `findSpecial` (wave 64, `CRATONVM_MH_SPECIAL_BY_OWNER_ID`).
    let special_owner =
        special_dispatch_owner(&*ctx, &dispatch_class, &class_name, refc_id, caller_id);
    let special_caller = special_caller_owner(&*ctx, caller_name.as_deref(), caller_id);
    let mh = alloc_method_handle_with_receiver(
        ctx,
        &dispatch_class,
        &method_name,
        &descriptor,
        MH_KIND_SPECIAL,
        special_receiver(caller_name.as_deref()),
        refc_id,
    )?;
    mh_record_special_owner(&*ctx, mh, special_owner);
    mh_record_special_caller(&*ctx, mh, special_caller);
    Ok(Some(Value::Object(Some(mh))))
}

/// Read the descriptor string for a Field reflection object. Prefers the
/// CratonVM extra-slot descriptor (matches `create_field_object` in
/// `lang_class.rs`), and falls back to deriving it from the `type` Class
/// mirror if needed.
pub(crate) fn read_field_descriptor_string(
    ctx: &dyn NativeContext,
    field_obj: cratonvm_types::ObjectRef,
) -> Cow<'static, str> {
    // The `type` field is a Class mirror — derive the descriptor from it
    // as a safe fallback (e.g. "J" for primitive long, "Ljava/lang/String;"
    // for references). This is the authoritative source in real JDK mode.
    if let Value::Object(Some(type_mirror)) = ctx.get_field_by_name(field_obj, "type") {
        let name = crate::lang_class::mirror_class_name(ctx, type_mirror).unwrap_or_default();
        if !name.is_empty() {
            return match name.as_str() {
                NAME_BOOLEAN => Cow::Borrowed(DESC_BOOLEAN),
                NAME_BYTE => Cow::Borrowed(DESC_BYTE),
                NAME_CHAR => Cow::Borrowed(DESC_CHAR),
                NAME_SHORT => Cow::Borrowed(DESC_SHORT),
                NAME_INT => Cow::Borrowed(DESC_INT),
                NAME_LONG => Cow::Borrowed(DESC_LONG),
                NAME_FLOAT => Cow::Borrowed(DESC_FLOAT),
                NAME_DOUBLE => Cow::Borrowed(DESC_DOUBLE),
                NAME_VOID => Cow::Borrowed(DESC_VOID),
                s if s.starts_with('[') => Cow::Owned(s.to_string()),
                s => Cow::Owned(format!("L{};", s.replace('.', "/"))),
            };
        }
    }
    Cow::Borrowed(DESC_OBJECT)
}

pub(crate) fn lookup_unreflect_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Lookup, args[1] = Field object.
    // Read Field fields via `get_field_by_name` / the `type` mirror so the
    // resolution lands on the real JDK `java.lang.reflect.Field` layout
    // (clazz/name/type at hierarchy-adjusted slots 2/4/5) rather than on
    // our legacy synthetic-slot layout. See the C5 fix in
    // `lang_class.rs::create_field_object` for details.
    // FIRST statement on purpose — see `lk_enforce_unreflect_access`.
    lk_enforce_unreflect_access(ctx, args, LkUnreflectKind::AccessibleWaives)?;
    const ACC_STATIC: i32 = 0x0008;
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        // `unreflectGetter/Setter(null)`: NPE, as the JDK body dereferences it first.
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => {
            return Err(no_such_field_error("", ""));
        }
    };
    if let Some(result) = lk_lambda_proxy_unreflect_field(ctx, args, field_obj, false) {
        return result;
    }

    let class_name = match ctx.get_field_by_name(field_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    let field_name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let field_desc = read_field_descriptor_string(ctx, field_obj);

    // Static getter takes no receiver; instance getter takes the owner.
    let desc = if (modifiers & ACC_STATIC) != 0 {
        format!("(){field_desc}")
    } else {
        format!("(L{class_name};){field_desc}")
    };
    let owner = if (modifiers & ACC_STATIC) != 0 {
        let m = member_clazz_mirror(ctx, field_obj);
        mh_static_owner_of_mirror(&*ctx, m)
    } else {
        None
    };
    // Stage 2's owner of an instance field (wave 63,
    // `CRATONVM_MH_OWNER_ID_AUTHORITY`): `Field.clazz`'s id.
    let field_owner = if (modifiers & ACC_STATIC) != 0 {
        None
    } else {
        let m = member_clazz_mirror(ctx, field_obj);
        mh_field_owner_of_mirror(&*ctx, m)
    };
    // A protected instance field of another package: the receiver is the
    // lookup class (wave 44, `L4W44ProtectedReceiver` row `unreflect-getter`).
    let restricted = if (modifiers & ACC_STATIC) != 0 {
        None
    } else {
        lk_unreflect_protected_receiver(&*ctx, args, field_obj, LkMember::Field(&field_name))
    };
    let mh = alloc_method_handle(ctx, &class_name, &field_name, &desc, MH_KIND_GETTER)?;
    mh_record_static_owner(ctx, mh, owner);
    mh_record_instance_owner(&*ctx, mh, field_owner);
    mh_record_minted_field_slot(&*ctx, mh, field_owner, &field_name);
    if let Some(receiver) = restricted {
        return lk_restrict_receiver_type(ctx, mh, &format!("(L{receiver};){field_desc}"));
    }
    Ok(Some(Value::Object(Some(mh))))
}

pub(crate) fn lookup_unreflect_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FIRST statement on purpose — see `lk_enforce_unreflect_access`.
    //
    // NOT enforced here, and still open: `unreflectSetter` must also refuse a
    // TRUSTED-final field ("fields which are both `static` and `final` may
    // never be set"). That is `MemberName.isTrustedFinalField`, a different
    // question from lookup modes, and guessing at it would refuse the
    // `setAccessible`-then-`unreflectSetter` idiom that deserialization
    // frameworks depend on. Recorded in W6-8 rather than written blind.
    lk_enforce_unreflect_access(ctx, args, LkUnreflectKind::AccessibleWaives)?;
    const ACC_STATIC: i32 = 0x0008;
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        // `unreflectGetter/Setter(null)`: NPE, as the JDK body dereferences it first.
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => {
            return Err(no_such_field_error("", ""));
        }
    };
    if let Some(result) = lk_lambda_proxy_unreflect_field(ctx, args, field_obj, true) {
        return result;
    }

    let class_name = match ctx.get_field_by_name(field_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    let field_name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let field_desc = read_field_descriptor_string(ctx, field_obj);

    // Static setter takes only the new value; instance takes (owner, value).
    let desc = if (modifiers & ACC_STATIC) != 0 {
        format!("({field_desc})V")
    } else {
        format!("(L{class_name};{field_desc})V")
    };
    let owner = if (modifiers & ACC_STATIC) != 0 {
        let m = member_clazz_mirror(ctx, field_obj);
        mh_static_owner_of_mirror(&*ctx, m)
    } else {
        None
    };
    // Stage 2's owner of an instance field (wave 63,
    // `CRATONVM_MH_OWNER_ID_AUTHORITY`): `Field.clazz`'s id.
    let field_owner = if (modifiers & ACC_STATIC) != 0 {
        None
    } else {
        let m = member_clazz_mirror(ctx, field_obj);
        mh_field_owner_of_mirror(&*ctx, m)
    };
    // As `unreflectGetter` (wave 44, row `unreflect-setter`).
    let restricted = if (modifiers & ACC_STATIC) != 0 {
        None
    } else {
        lk_unreflect_protected_receiver(&*ctx, args, field_obj, LkMember::Field(&field_name))
    };
    let mh = alloc_method_handle(ctx, &class_name, &field_name, &desc, MH_KIND_SETTER)?;
    mh_record_static_owner(ctx, mh, owner);
    mh_record_instance_owner(&*ctx, mh, field_owner);
    mh_record_minted_field_slot(&*ctx, mh, field_owner, &field_name);
    if let Some(receiver) = restricted {
        return lk_restrict_receiver_type(ctx, mh, &format!("(L{receiver};{field_desc})V"));
    }
    Ok(Some(Value::Object(Some(mh))))
}

/// `MethodHandles.Lookup.unreflectVarHandle(Field)`.
pub(crate) fn lookup_unreflect_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FIRST statement on purpose — see `lk_enforce_unreflect_access`. This is
    // the one arm that must NOT honour the `accessible` flag: "Access checking
    // is performed immediately on behalf of the lookup class, regardless of the
    // value of the field's `accessible` flag."
    lk_enforce_unreflect_access(ctx, args, LkUnreflectKind::AccessibleIgnored)?;
    const ACC_STATIC: i32 = 0x0008;
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Err(no_such_field_error("", "")),
    };
    let class_mirror = match ctx.get_field_by_name(field_obj, "clazz") {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let class_name = match class_mirror {
        Some(m) => mirror_class_name(ctx, m).unwrap_or_default(),
        None => String::new(),
    };
    let field_name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(n)) => ctx.read_string(n).unwrap_or_default(),
        _ => String::new(),
    };
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let field_desc = read_field_descriptor_string(ctx, field_obj);
    if class_name.is_empty() || field_name.is_empty() {
        return Err(no_such_field_error(&class_name, &field_name));
    }
    // Read before the load below, a GC point for every reference in `args`.
    // No `accessible` exemption: this finder always checks as the lookup
    // (see the first statement), so a `setAccessible(true)` field is
    // restricted too.
    let from = lk_from_of(&*ctx, args);
    // `unreflectVarHandle` over a `final` field is read-only exactly as
    // `findVarHandle`/`findStaticVarHandle` are (JDK: "If the field is declared
    // final, then the write, atomic update, numeric atomic update, and bitwise
    // atomic update access modes are not supported"), and regardless of the
    // `Field`'s accessible flag. This door never marked it, so
    // `lookup.unreflectVarHandle(finalField).set(h, 5)` stored into the final
    // field. The modifiers are the `Field`'s own, so no hierarchy walk is needed.
    let is_final = (modifiers & i32::from(ACC_FINAL)) != 0;
    if (modifiers & ACC_STATIC) != 0 {
        let owner = mh_static_owner_of_mirror(&*ctx, class_mirror);
        let vh = alloc_static_var_handle(ctx, &class_name, &field_name, &field_desc, owner)?;
        let vh = if is_final {
            vh_mark_read_only(ctx, vh)
        } else {
            vh
        };
        return Ok(Some(Value::Object(Some(vh))));
    }
    // `-1` is the "re-resolve by name at access time" sentinel; `0` is slot 0.
    // The `Field` object proves the field EXISTS, so no `lookup_require_field`
    // gate here — but `resolve_field_index` can still miss it (an inherited or
    // otherwise not-directly-resolvable field), and answering slot 0 for that
    // aliases an unrelated field of the receiver.
    let field_index = match ctx.resolve_field_index(&class_name, &field_name) {
        Some(idx) => idx as i32,
        None => -1,
    };
    let class_id = match class_mirror.and_then(|m| mirror_class_id(ctx, m)) {
        Some(id) => id,
        None => match ctx.class_id_by_name(&class_name) {
            Some(id) => id,
            // LOAD only: an instance field's receiver initializes the class.
            None => match lookup_load_class(ctx, &class_name) {
                Ok(Some(id)) => id,
                _ => return Err(no_such_field_error(&class_name, &field_name)),
            },
        },
    };
    // `getFieldVarHandleCommon` with `refc` the declaring class (wave 45).
    let (coord_class, coord_id) = vh_protected_coordinate(&*ctx, from, class_id, &field_name)
        .unwrap_or_else(|| (class_name.clone(), class_id));
    let vh = alloc_instance_var_handle(
        ctx,
        &coord_class,
        &field_name,
        &field_desc,
        field_index,
        coord_id,
        Some(class_id),
    )?;
    let vh = if is_final {
        vh_mark_read_only(ctx, vh)
    } else {
        vh
    };
    Ok(Some(Value::Object(Some(vh))))
}

/// `VarHandle.toMethodHandle(AccessMode)` -- see the registration comment.
///
/// Only the plain read/write access modes are expressible as one of our
/// synthetic getter/setter MethodHandles. Anything else (the CAS and
/// get-and-update families) raises `UnsupportedOperationException` rather than
/// handing back a handle that would silently do the wrong thing.
/// `VarHandle.isAccessModeSupported(AccessMode)`: whether `mode` (the
/// `AccessMode` constant's name, `GET_AND_ADD`) is one the handle's natives
/// perform rather than refuse with `UnsupportedOperationException`.
///
/// * field and array handles: every read, write and compare-and-set mode;
///   `GET_AND_ADD*` for a numeric variable, `GET_AND_BITWISE*` for an integral
///   or `boolean` one (`vh_check_access_mode_supported`); a handle over a
///   final field reads only (`vh_check_read_only`);
/// * a `byte[]` view: `GET` and `SET` only, and a `ByteBuffer` view by its
///   element type (`vh_view_mode_refusal`);
/// * a handle this VM does not model (the FFM segment and layout handles):
///   `true`, which is what the JDK answers for their aligned layouts.
pub(crate) fn varhandle_is_access_mode_supported(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vh = obj_arg(args, 0)?;
    // `CRATONVM_VH_REAL_VARFORM` (interpreter round i6 wave 123, lane L4): the
    // JDK's own body, `vform.getMemberNameOrNull(mode.ordinal()) != null`, over
    // the real `VarForm` this VM minted for the handle. A `null` mode is that
    // body's `NullPointerException` too.
    if vh_has_real_varform_of_ours(ctx, vh) {
        return ctx.invoke_special_bytecode_only(
            "java/lang/invoke/VarHandle",
            "isAccessModeSupported",
            "(Ljava/lang/invoke/VarHandle$AccessMode;)Z",
            args,
        );
    }
    let mode = match args.get(1) {
        Some(Value::Object(Some(mode))) => access_mode_name(ctx, *mode),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let meta = vh_meta_get(ctx, vh);
    let (kind, first, read_only) = match meta.as_deref() {
        Some(m) => (
            m.kind,
            m.field_desc.as_bytes().first().copied().unwrap_or(b'L'),
            m.read_only,
        ),
        None => match real_array_var_handle_descriptors(ctx, vh) {
            Some((value_desc, _)) => (
                VH_KIND_ARRAY,
                value_desc.as_bytes().first().copied().unwrap_or(b'L'),
                false,
            ),
            None => {
                // SBR3 lane ffm: a REAL `SegmentVarHandle` answers by its
                // layout, as the JDK's `accessModesBitMask` does.
                if is_segment_var_handle(ctx, vh) {
                    if let Some(answer) = segment_vh_mode_supported(ctx, vh, &mode) {
                        return Ok(Some(Value::Int(answer as i32)));
                    }
                }
                return Ok(Some(Value::Int(1)));
            }
        },
    };
    Ok(Some(Value::Int(
        vh_access_mode_supported(kind, first, read_only, &mode) as i32,
    )))
}

/// JIT round 20 wave 8 (lane huntFFM): an ORDERED access mode (`getVolatile`,
/// `getAcquire`, `getOpaque`, `setVolatile`, `setRelease`, `setOpaque`) on a
/// real `SegmentVarHandle` whose form has only `get` / `set`
/// ([`SegVhPlan::aligned_form`]) is `UnsupportedOperationException(<mode>)`,
/// as on HotSpot 25 (`JAVA_INT_UNALIGNED.varHandle().getVolatile(seg, 0L)`).
/// The get / set natives used to serve it as a plain access. `None`: `mode`
/// is `get` / `set`, the form supports it, or the plan cannot be read (the
/// access road then decides, as before).
pub(crate) fn segment_vh_ordered_mode_refusal(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    mode: &str,
) -> Option<MethodCallFailed> {
    if mode == "get" || mode == "set" || !segment_vh_checks() || !segment_vh_form_enabled() {
        return None;
    }
    let plan = seg_vh_plan(ctx, vh)?;
    (!plan.aligned_form).then(|| {
        RuntimeError::UnsupportedOperationException {
            message: mode.to_string(),
        }
        .into()
    })
}

/// Whether a real `SegmentVarHandle` supports the access mode named `mode`
/// (`GET_AND_ADD`): an UNALIGNED layout (alignment below its width) supports
/// the plain `GET` and `SET` only; an aligned one every read and write mode,
/// the compare-and-set and `getAndSet` families on `int`/`long`/`float`/
/// `double`, and `getAndAdd` / `getAndBitwise*` on `int`/`long`. `None` when
/// the handle's layout cannot be read.
pub(crate) fn segment_vh_mode_supported(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    mode: &str,
) -> Option<bool> {
    let plan = seg_vh_plan(ctx, vh)?;
    let shape = plan.shape;
    if matches!(mode, "GET" | "SET") {
        return Some(true);
    }
    // JIT round 20 wave 8 (lane huntFFM): the form, not `enclosing`'s
    // alignment (see `SegVhPlan::aligned_form`).
    if !plan.aligned_form {
        return Some(false);
    }
    let integral = matches!(shape, SegShape::Int | SegShape::Long);
    let floating = matches!(shape, SegShape::Float | SegShape::Double);
    Some(
        if mode.starts_with("GET_AND_ADD") || mode.starts_with("GET_AND_BITWISE") {
            integral
        } else if mode.starts_with("COMPARE_AND")
            || mode.starts_with("WEAK_COMPARE_AND")
            || mode.starts_with("GET_AND_SET")
        {
            integral || floating
        } else {
            true
        },
    )
}

/// `VarHandle.describeConstable()`: the `VarHandleDesc` JDK 25's own field
/// and array handle classes answer (`VarHandleInts.FieldInstanceReadOnly`,
/// `FieldStaticReadOnly`, `Array` override it), built from the side-table row
/// this VM keeps instead of their fields:
///
///  * an instance field: `VarHandleDesc.ofField(<receiver>, name, <type>)`,
///    the receiver being the handle's coordinate (the lookup class for a
///    restricted protected field, as the JDK's `receiverType` is);
///  * a static field: `VarHandleDesc.ofStaticField(<declaring>, name, <type>)`;
///  * an array element: `VarHandleDesc.ofArray(<array type>)`.
///
/// `Optional.empty()` for every other handle (byte views, FFM layouts, a JDK
/// array handle without a row: the JDK classes of the views answer empty too)
/// and for a hidden declaring class (`Class.describeConstable` is empty for
/// one). Measured on HotSpot 25: `L4W45VarHandleQueries` rows `describe-*`;
/// every CratonVM handle printed `Optional.empty`, in both modes.
pub(crate) fn varhandle_describe_constable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vh = obj_arg(args, 0)?;
    let meta = vh_meta_get(ctx, vh);
    let shape = meta.as_deref().and_then(|m| {
        if m.class_name.is_empty() {
            return None;
        }
        let owner_id = if m.class_id != 0 {
            Some(ClassId::new(m.class_id))
        } else {
            ctx.class_id_by_name(&m.class_name)
        };
        // A static handle names the field's DECLARING class
        // (`VarHandles.makeFieldHandle`: `f.getDeclaringClass()`), which for
        // an inherited field is a superclass of the class it was found in.
        let owner_id = match (m.kind, owner_id) {
            (VH_KIND_STATIC, Some(cid)) => {
                Some(vh_static_declaring_class(&*ctx, cid, &m.field_name))
            }
            (_, id) => id,
        };
        if owner_id.is_some_and(|cid| ctx.is_class_hidden(cid)) {
            return None;
        }
        let owner_name = match (m.kind, owner_id) {
            (VH_KIND_STATIC, Some(cid)) => ctx
                .class_name_of_id(cid)
                .unwrap_or_else(|| m.class_name.clone()),
            _ => m.class_name.clone(),
        };
        let owner = class_name_to_descriptor(&owner_name).into_owned();
        match m.kind {
            VH_KIND_INSTANCE if !m.field_name.is_empty() => Some((
                "ofField",
                owner,
                Some((m.field_name.clone(), m.field_desc.clone())),
            )),
            VH_KIND_STATIC if !m.field_name.is_empty() => Some((
                "ofStaticField",
                owner,
                Some((m.field_name.clone(), m.field_desc.clone())),
            )),
            VH_KIND_ARRAY if owner.starts_with('[') => Some(("ofArray", owner, None)),
            _ => None,
        }
    });
    let Some((factory, owner_desc, field)) = shape else {
        return ctx.invoke_static("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]);
    };
    const CLASS_DESC: &str = "java/lang/constant/ClassDesc";
    const OF_DESCRIPTOR: &str = "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;";
    const VH_DESC: &str = "java/lang/invoke/VarHandle$VarHandleDesc";
    // Each step runs Java (a GC point): every reference made so far is
    // pinned across it and re-read after.
    let owner_str = ctx.create_string_uninterned(&owner_desc);
    let owner_arg = [Value::Object(Some(owner_str))];
    let owner_cd = match ctx.invoke_static(CLASS_DESC, "ofDescriptor", OF_DESCRIPTOR, &owner_arg)? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return ctx.invoke_static("java/util/Optional", "empty", "()Ljava/util/Optional;", &[])
        }
    };
    let base = ctx.pin_native_root(owner_cd);
    let desc = match field {
        None => {
            let owner_cd = ctx.read_native_pin(base, owner_cd);
            let r = ctx.invoke_static(
                VH_DESC,
                factory,
                "(Ljava/lang/constant/ClassDesc;)Ljava/lang/invoke/VarHandle$VarHandleDesc;",
                &[Value::Object(Some(owner_cd))],
            );
            ctx.unpin_native_roots(base);
            r?
        }
        Some((name, type_desc)) => {
            let name_str = ctx.intern_string(&name);
            let name_pin = ctx.pin_native_root(name_str);
            let type_str = ctx.create_string_uninterned(&type_desc);
            let type_cd = match ctx.invoke_static(
                CLASS_DESC,
                "ofDescriptor",
                OF_DESCRIPTOR,
                &[Value::Object(Some(type_str))],
            ) {
                Ok(Some(Value::Object(Some(t)))) => t,
                Ok(_) => {
                    ctx.unpin_native_roots(base);
                    return ctx.invoke_static(
                        "java/util/Optional",
                        "empty",
                        "()Ljava/util/Optional;",
                        &[],
                    );
                }
                Err(e) => {
                    ctx.unpin_native_roots(base);
                    return Err(e);
                }
            };
            let owner_cd = ctx.read_native_pin(base, owner_cd);
            let name_str = ctx.read_native_pin(name_pin, name_str);
            let r = ctx.invoke_static(
                VH_DESC,
                factory,
                "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/ClassDesc;)Ljava/lang/invoke/VarHandle$VarHandleDesc;",
                &[
                    Value::Object(Some(owner_cd)),
                    Value::Object(Some(name_str)),
                    Value::Object(Some(type_cd)),
                ],
            );
            ctx.unpin_native_roots(base);
            r?
        }
    };
    ctx.invoke_static(
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[desc.unwrap_or(Value::Object(None))],
    )
}

/// The class on `cid`'s superclass chain that declares the static field
/// `name`, else `cid` (an interface constant, or a field the walk cannot see).
pub(crate) fn vh_static_declaring_class(ctx: &dyn NativeContext, cid: ClassId, name: &str) -> ClassId {
    const ACC_STATIC: u16 = 0x0008;
    let mut cur = cid;
    for _ in 0..256 {
        if ctx
            .declared_fields(cur)
            .iter()
            .any(|f| f.name == name && f.access_flags & ACC_STATIC != 0)
        {
            return cur;
        }
        match ctx.superclass_of(cur) {
            Some(parent) if parent != cur => cur = parent,
            _ => break,
        }
    }
    cid
}

/// The rule [`varhandle_is_access_mode_supported`] answers with, for a handle
/// of side-table `kind` over a variable whose descriptor starts with `first`.
pub(crate) fn vh_access_mode_supported(kind: i32, first: u8, read_only: bool, mode: &str) -> bool {
    let plain = mode == "GET" || mode == "SET";
    let read = mode.starts_with("GET") && !mode.starts_with("GET_AND");
    let ordered = matches!(
        mode,
        "GET_VOLATILE"
            | "SET_VOLATILE"
            | "GET_ACQUIRE"
            | "SET_RELEASE"
            | "GET_OPAQUE"
            | "SET_OPAQUE"
    );
    let arithmetic = mode.starts_with("GET_AND_ADD");
    let bitwise = mode.starts_with("GET_AND_BITWISE");
    match kind {
        VH_KIND_BYTE_VIEW_LE | VH_KIND_BYTE_VIEW_BE => plain,
        VH_KIND_BYTE_BUFFER_VIEW_LE | VH_KIND_BYTE_BUFFER_VIEW_BE => match first {
            b'I' | b'J' => true,
            b'F' | b'D' => !(arithmetic || bitwise),
            _ => plain || ordered,
        },
        VH_KIND_INSTANCE | VH_KIND_STATIC | VH_KIND_ARRAY => {
            if read_only && !read {
                return false;
            }
            if arithmetic {
                matches!(first, b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
            } else if bitwise {
                matches!(first, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J')
            } else {
                true
            }
        }
        _ => true,
    }
}

pub(crate) fn varhandle_to_method_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vh = match args.first() {
        Some(Value::Object(Some(v))) => *v,
        _ => {
            return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
                message: Some("VarHandle.toMethodHandle on a null VarHandle".to_string()),
            }))
        }
    };
    let mode = match args.get(1) {
        Some(Value::Object(Some(mode))) => *mode,
        _ => {
            return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
                message: None,
            }))
        }
    };
    // `CRATONVM_VH_REAL_VARFORM` (interpreter round i6 wave 123, lane L4): a
    // handle carrying the real `VarForm` this VM minted for it runs the JDK's
    // own body, `getMethodHandle(mode).bindTo(asDirect())` over the form's
    // `implClass` method, or the bound invoker of an unsupported mode.
    if vh_has_real_varform_of_ours(ctx, vh) {
        return ctx.invoke_special_bytecode_only(
            "java/lang/invoke/VarHandle",
            "toMethodHandle",
            "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodHandle;",
            args,
        );
    }
    let mode_name = access_mode_name(ctx, mode);
    // Interpreter round i5 wave 100, lane L4 (probe
    // `L4W100VarHandleToMethodHandleReveal`; `CRATONVM_VH_TO_METHOD_HANDLE_BOUND`,
    // default ON, `=0` restores the accessor form below): JDK 25's
    // `toMethodHandle` answers `getMethodHandle(mode).bindTo(asDirect())`, a
    // BOUND handle (`getMethodHandleUncached`: "must not be publicly exposed
    // directly otherwise it can be cracked"), so `Lookup.revealDirect` of it is
    // `IllegalArgumentException: not a direct method handle`. The accessor form
    // is a direct getter / setter, which `revealDirect` cracked as
    // `REF_getField` / `REF_putField`. With the switch on, GET and SET take the
    // bound mode invoker below like every other mode.
    // A field handle's PLAIN read, and its plain write when the field is not
    // final, keep their field-accessor form. The ordered modes
    // (`GET_VOLATILE`, `SET_RELEASE`, `GET_ACQUIRE`, `SET_OPAQUE`, ...) are
    // the mode's invoker below like every other mode: the accessor is a plain
    // load or store, which dropped the ordering the mode names on a
    // non-volatile field (interpreter round i1 wave 38, lane L4; item 3 of
    // `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`).
    let accessor_form = !crate::nbflags().vh_to_method_handle_bound;
    if let Some(meta) = vh_meta_get(ctx, vh).filter(|_| accessor_form) {
        let is_static = match meta.kind {
            VH_KIND_INSTANCE => Some(false),
            VH_KIND_STATIC => Some(true),
            _ => None,
        };
        let is_get = mode_name == "GET";
        let is_set = mode_name == "SET" && !meta.read_only;
        if let Some(is_static) = is_static.filter(|_| is_get || is_set) {
            let class = meta.class_name.clone();
            let field = meta.field_name.clone();
            let fdesc = meta.field_desc.clone();
            let receiver = if is_static {
                String::new()
            } else {
                format!("L{class};")
            };
            let (kind, desc) = if is_get {
                (MH_KIND_GETTER, format!("({receiver}){fdesc}"))
            } else {
                (MH_KIND_SETTER, format!("({receiver}{fdesc})V"))
            };
            // An instance handle whose coordinate is not its field's holder
            // (a restricted protected field): the accessor is minted for the
            // holder, where its dispatch looks the name up, and typed with the
            // coordinate, as `findGetter`'s restriction is (wave 46, lane L4).
            let holder = if is_static
                || meta.holder_class_id == 0
                || meta.holder_class_id == meta.class_id
            {
                None
            } else {
                ctx.class_name_of_id(ClassId::new(meta.holder_class_id))
            };
            if let Some(holder) = holder {
                let holder_desc = if is_get {
                    format!("(L{holder};){fdesc}")
                } else {
                    format!("(L{holder};{fdesc})V")
                };
                let mh = alloc_method_handle(ctx, &holder, &field, &holder_desc, kind)?;
                return lk_restrict_receiver_type(ctx, mh, &desc);
            }
            let mh = alloc_method_handle(ctx, &class, &field, &desc, kind)?;
            return Ok(Some(Value::Object(Some(mh))));
        }
    }
    // Every other mode, and every array or view handle: the access mode's
    // own method called on this handle, bound to it (interpreter round i1
    // wave 32). That is HotSpot's shape: `toMethodHandle` binds the handle
    // into the mode's invoker, typed `accessModeType(mode)`, so a
    // read-modify-write mode is a handle (`GET_AND_ADD` on an `int` field of
    // `H` is `(H,int)int`), and an unsupported mode (a write to a final field)
    // throws `UnsupportedOperationException` when invoked, not here.
    let mode_type = match ctx.invoke_virtual(
        vh,
        "accessModeType",
        "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(mode))],
    )? {
        Some(Value::Object(Some(mt))) => mt,
        _ => {
            return Err(MethodCallFailed::from(
                RuntimeError::UnsupportedOperationException { message: mode_name },
            ))
        }
    };
    let desc = match ctx.invoke_virtual(
        mode_type,
        "toMethodDescriptorString",
        "()Ljava/lang/String;",
        &[],
    )? {
        Some(Value::Object(Some(d))) => ctx.read_string(d).unwrap_or_default(),
        _ => String::new(),
    };
    let method = access_mode_method_name(&mode_name);
    if desc.is_empty() || method.is_empty() {
        return Err(MethodCallFailed::from(
            RuntimeError::UnsupportedOperationException { message: mode_name },
        ));
    }
    let invoker = alloc_method_handle(
        ctx,
        "java/lang/invoke/VarHandle",
        &method,
        &desc,
        MH_KIND_VIRTUAL,
    )?;
    ctx.invoke_virtual(
        invoker,
        "bindTo",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(vh))],
    )
}

/// `VarHandle$AccessMode`'s method name for its enum name:
/// `GET_AND_BITWISE_OR_RELEASE` is `getAndBitwiseOrRelease`.
pub(crate) fn access_mode_method_name(mode_name: &str) -> String {
    let mut out = String::with_capacity(mode_name.len());
    for (i, part) in mode_name.split('_').enumerate() {
        let lower = part.to_ascii_lowercase();
        if i == 0 {
            out.push_str(&lower);
        } else {
            let mut chars = lower.chars();
            if let Some(c) = chars.next() {
                out.push(c.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

/// Read a `VarHandle$AccessMode` constant's enum name (`"GET"`, `"SET"`, ...).
pub(crate) fn access_mode_name(ctx: &mut dyn NativeContext, mode: ObjectRef) -> String {
    if let Value::Object(Some(n)) = ctx.get_field_by_name(mode, "name") {
        if let Some(s) = ctx.read_string(n) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    match ctx.invoke_virtual(mode, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(n)))) => ctx.read_string(n).unwrap_or_default(),
        _ => String::new(),
    }
}

pub(crate) fn lookup_unreflect_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FIRST statement on purpose — see `lk_enforce_unreflect_access`.
    lk_enforce_unreflect_access(ctx, args, LkUnreflectKind::AccessibleWaives)?;
    // args[0] = Lookup, args[1] = java.lang.reflect.Constructor
    let ctor_obj = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        // `unreflectConstructor(null)`: the JDK body dereferences it first.
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => {
            return Err(no_such_method_error("", "<init>", ""));
        }
    };
    // Read the declaring class + the `(...)V` constructor descriptor straight
    // from the Constructor reflection object. `method_class_name_desc` handles
    // the `<init>` name and coerces the return to V (a Constructor has no
    // `returnType` field). Build a synthetic CONSTRUCTOR MethodHandle the same
    // way `findConstructor` does, so the invoke/invokeExact native can dispatch
    // it (vs the real-JDK DirectMethodHandle$Constructor it otherwise becomes).
    let (class_id, _name, desc) = match crate::lang_class::method_class_name_desc(ctx, ctor_obj) {
        Some(v) => v,
        None => {
            return Err(no_such_method_error("", "<init>", ""));
        }
    };
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    // LOAD only: `mh_dispatch`'s constructor arm initializes at the first call.
    let _ = lookup_load_class(ctx, &class_name);
    let mh = alloc_method_handle(ctx, &class_name, "<init>", &desc, MH_KIND_CONSTRUCTOR)?;
    // See the matching comment in lookup_find_constructor: stash the
    // already-resolved class_id (from the Constructor reflection
    // object's own clazz mirror) so dispatch doesn't re-resolve class_name
    // through the loader-blind, one-copy-per-name global map.
    ctx.set_field(mh, MH_BOUND, Value::Int(class_id.as_u32() as i32));
    Ok(Some(Value::Object(Some(mh))))
}

// ---------------------------------------------------------------------------
// T2.8.11 — MethodHandles.permuteArguments
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// `--jdk-only` argument checks of the `MethodHandles` combinators served here
// (interpreter round i1 wave 42, lane L4, review; probe
// `tools/probes/interp/L4/L4W42CombinatorChecks.java`). The natives answered a
// `null` target / filter / combiner / test with a `null` handle or the bare
// target, and built an adapter over types HotSpot refuses. Each check below is
// the JDK 25 method's own (`MethodHandles.filterArguments`,
// `filterReturnValueChecks`, `foldArgumentChecks`, `collectArgumentsChecks`,
// `guardWithTest`, `catchException`, `permuteArgumentChecks`), in its order,
// with its message.
//
// A `null` is refused whatever the handles are. A TYPE is compared only when
// every handle involved is a direct `findStatic` / `findVirtual` handle whose
// declared `type` is its member's own ([`combinator_direct_type`]): an
// adapter's `type` here can differ from HotSpot's (the guard adapter keeps its
// target's raw return type, see `mhs_guard_with_test`), and a refusal HotSpot
// does not make would break working code. Every other case is an accept, as
// before. `--compatible` is unchanged.
// ---------------------------------------------------------------------------

/// The declared `type()` descriptor of a DIRECT `findStatic` /
/// `findVirtual` handle whose type is its member's own (not bound, not an
/// `asType` copy), or `None` for any other handle.
pub(crate) fn combinator_direct_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    // Width guard (a real JDK handle has none of this model's slots).
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return None;
    }
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    // `constant` / `identity` / `zero` / `empty` (and a `bindTo` copy of
    // one, whose `type` drops the bound parameter, wave 40) are minted with
    // their exact `type` (the rule `cs_target_type` already trusts), so a
    // combinator over one is judged too (interpreter round i1 wave 47, lane
    // L4, probe `L4W47CombinatorChecksOverConstants`): HotSpot refused
    // `filterReturnValue(f, identity(String))` over an `int` result, which
    // built an adapter here.
    if matches!(kind, MH_KIND_CONSTANT | MH_KIND_IDENTITY | MH_KIND_THROW) {
        return mh_declared_descriptor(ctx, mh);
    }
    if !matches!(kind, MH_KIND_STATIC | MH_KIND_VIRTUAL)
        || matches!(ctx.get_field(mh, MH_BOUND), Value::Object(Some(_)))
    {
        return None;
    }
    let class = mh_read_class(&*ctx, mh)?;
    let desc = mh_read_desc(&*ctx, mh)?;
    if class.is_empty() || class.starts_with('[') {
        return None;
    }
    let own = if kind == MH_KIND_VIRTUAL {
        format!("(L{class};{}", desc.strip_prefix('(')?)
    } else {
        desc
    };
    (mh_declared_descriptor(ctx, mh)? == own).then_some(own)
}

/// `args[i]` as a non-null reference.
pub(crate) fn combinator_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

pub(crate) fn combinator_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

/// `CRATONVM_MH_COMBINATOR_NULL_TEXTS` (parity, default ON, `=0` off;
/// interpreter round i7 wave 141, lane L4, `L4W141CombinatorNullTexts`): the
/// combinator refusals raise the helpful `NullPointerException` JDK 25's
/// bodies raise for a null handle / type / array argument, in the JDK's
/// dereference order (measured on HotSpot 25.0.3). `=0`: the message-less NPE,
/// as before. Read only when an argument is null.
pub(crate) fn mh_combinator_null_texts_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_COMBINATOR_NULL_TEXTS")
}

/// The helpful NPE of a JDK combinator body that dereferences the null
/// parameter `param` as `<what>` (`Cannot invoke "<what>" because "<param>" is
/// null`), or the message-less one under `CRATONVM_MH_COMBINATOR_NULL_TEXTS=0`.
pub(crate) fn combinator_npe_invoking(what: &str, param: &str) -> MethodCallFailed {
    if !mh_combinator_null_texts_enabled() {
        return combinator_npe();
    }
    RuntimeError::NullPointerException {
        message: Some(format!("Cannot invoke \"{what}\" because \"{param}\" is null")),
    }
    .into()
}

/// [`combinator_npe_invoking`] for a null `MethodHandle` parameter, whose first
/// use in every JDK combinator body is `param.type()`.
pub(crate) fn combinator_npe_handle(param: &str) -> MethodCallFailed {
    combinator_npe_invoking("java.lang.invoke.MethodHandle.type()", param)
}

/// The first null of `named` (handle parameters in the JDK body's dereference
/// order) as [`combinator_npe_handle`]; `None` when none is null.
pub(crate) fn combinator_first_null_handle(named: &[(Option<ObjectRef>, &str)]) -> Option<MethodCallFailed> {
    named
        .iter()
        .find(|(h, _)| h.is_none())
        .map(|(_, param)| combinator_npe_handle(param))
}

pub(crate) fn combinator_iae(message: String) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException { message }.into()
}

/// `newIllegalArgumentException(what, t1, t2)` over two method types:
/// `what: (A)R, (B)S`.
pub(crate) fn combinator_types_iae(
    ctx: &dyn NativeContext,
    what: &str,
    t1: &str,
    t2: &str,
) -> Option<MethodCallFailed> {
    let a = method_type_display_in(ctx, t1)?;
    let b = method_type_display_in(ctx, t2)?;
    Some(combinator_iae(format!("{what}: {a}, {b}")))
}

/// `misMatchedTypes(what, t1, t2)`: `what must match: (A)R != (B)S`.
pub(crate) fn combinator_mismatch_iae(
    ctx: &dyn NativeContext,
    what: &str,
    t1: &str,
    t2: &str,
) -> Option<MethodCallFailed> {
    let a = method_type_display_in(ctx, t1)?;
    let b = method_type_display_in(ctx, t2)?;
    Some(combinator_iae(format!("{what} must match: {a} != {b}")))
}

/// `filterArguments(target, pos, filters...)`: `filterArgumentsCheckArity`,
/// then each non-null filter LAST FIRST against the target's own type
/// (`filterArgumentChecks`; a negative position reads `parameterType(-1)`).
pub(crate) fn filter_arguments_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallFailed> {
    let Some(target) = combinator_arg(args, 0) else {
        return Some(combinator_npe_handle("target"));
    };
    let Some(filters) = combinator_arg(args, 2) else {
        // `filterArgumentsCheckArity`'s `filters.length` (wave 141).
        if !mh_combinator_null_texts_enabled() {
            return Some(combinator_npe());
        }
        return Some(
            RuntimeError::NullPointerException {
                message: Some("Cannot read the array length because \"filters\" is null".to_string()),
            }
            .into(),
        );
    };
    let pos = match args.get(1) {
        Some(Value::Int(p)) => i64::from(*p),
        _ => return None,
    };
    let t = combinator_direct_type(ctx, target)?;
    let (tp, _) = split_descriptor_params(&t)?;
    let n = ctx.array_length(filters);
    if pos + n as i64 > tp.len() as i64 {
        return Some(combinator_iae("too many filters".to_string()));
    }
    for i in (0..n).rev() {
        let Value::Object(Some(filter)) = ctx.get_array_element(filters, i) else {
            continue;
        };
        let f = combinator_direct_type(ctx, filter)?;
        let (fp, fr) = split_descriptor_params(&f)?;
        let at = pos + i as i64;
        let mismatch = if fp.len() != 1 {
            true
        } else if at < 0 {
            return Some(RuntimeError::aioobe(at as i32, tp.len() as i32).into());
        } else {
            tp.get(at as usize)? != &fr
        };
        if mismatch {
            return combinator_types_iae(&*ctx, "target and filter types do not match", &t, &f);
        }
    }
    None
}

/// `filterReturnValue(target, filter)`: `filterReturnValueChecks`.
pub(crate) fn filter_return_value_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallFailed> {
    let (Some(target), Some(filter)) = (combinator_arg(args, 0), combinator_arg(args, 1)) else {
        return combinator_first_null_handle(&[
            (combinator_arg(args, 0), "target"),
            (combinator_arg(args, 1), "filter"),
        ]);
    };
    let t = combinator_direct_type(ctx, target)?;
    let f = combinator_direct_type(ctx, filter)?;
    let (_, tr) = split_descriptor_params(&t)?;
    let (fp, _) = split_descriptor_params(&f)?;
    let bad = match fp.as_slice() {
        [] => tr != "V",
        [p] => *p != tr,
        _ => true,
    };
    if bad {
        return combinator_types_iae(&*ctx, "target and filter types do not match", &t, &f);
    }
    None
}

/// `foldArguments(target, pos, combiner)`: `foldArgumentChecks`.
pub(crate) fn fold_arguments_refusal(
    ctx: &mut dyn NativeContext,
    target: Option<Value>,
    pos: i32,
    combiner: Option<Value>,
) -> Option<MethodCallFailed> {
    let as_handle = |v: Option<Value>| match v {
        Some(Value::Object(Some(h))) => Some(h),
        _ => None,
    };
    let (Some(target), Some(combiner)) = (as_handle(target), as_handle(combiner)) else {
        return combinator_first_null_handle(&[
            (as_handle(target), "target"),
            (as_handle(combiner), "combiner"),
        ]);
    };
    let t = combinator_direct_type(ctx, target)?;
    let c = combinator_direct_type(ctx, combiner)?;
    let (tp, _) = split_descriptor_params(&t)?;
    let (cp, cr) = split_descriptor_params(&c)?;
    let fold_vals: i64 = if cr == "V" { 0 } else { 1 };
    let after = i64::from(pos) + fold_vals;
    if after < 0 || pos < 0 {
        // Interpreter round i7 wave 141, lane L4 (`CRATONVM_MH_COMBINATOR_NULL_TEXTS`,
        // `L4W141CombinatorTexts` row `fold-neg-pos`): `foldArgumentChecks`
        // reads `targetType.parameterType(i + after)` for each combiner
        // parameter and then `parameterType(pos)` for a value combiner; the
        // first negative index is `ArrayIndexOutOfBoundsException: Index -1 out
        // of bounds for length <n>`, as HotSpot raises it. A shape that reads
        // no negative index is left as before.
        if !mh_combinator_null_texts_enabled() {
            return None;
        }
        let n = tp.len() as i64;
        if n < after + cp.len() as i64 {
            return combinator_mismatch_iae(&*ctx, "target and combiner types", &t, &c);
        }
        for (i, p) in cp.iter().enumerate() {
            let at = i as i64 + after;
            if at < 0 {
                return Some(RuntimeError::aioobe(at as i32, n as i32).into());
            }
            if tp.get(at as usize) != Some(p) {
                return combinator_mismatch_iae(&*ctx, "target and combiner types", &t, &c);
            }
        }
        if fold_vals != 0 {
            return Some(RuntimeError::aioobe(pos, n as i32).into());
        }
        return None;
    }
    let mut ok = tp.len() as i64 >= after + cp.len() as i64;
    if ok {
        ok = cp
            .iter()
            .enumerate()
            .all(|(i, p)| tp.get(i + after as usize) == Some(p));
    }
    if ok && fold_vals != 0 && tp.get(pos as usize) != Some(&cr) {
        ok = false;
    }
    if ok {
        return None;
    }
    combinator_mismatch_iae(&*ctx, "target and combiner types", &t, &c)
}

/// `collectArguments(target, pos, filter)`: `collectArgumentsChecks`.
pub(crate) fn collect_arguments_refusal(
    ctx: &mut dyn NativeContext,
    target: Option<Value>,
    pos: i32,
    filter: Option<Value>,
) -> Option<MethodCallFailed> {
    let as_handle = |v: Option<Value>| match v {
        Some(Value::Object(Some(h))) => Some(h),
        _ => None,
    };
    let (Some(target), Some(filter)) = (as_handle(target), as_handle(filter)) else {
        return combinator_first_null_handle(&[
            (as_handle(target), "target"),
            (as_handle(filter), "filter"),
        ]);
    };
    let t = combinator_direct_type(ctx, target)?;
    let f = combinator_direct_type(ctx, filter)?;
    let (tp, _) = split_descriptor_params(&t)?;
    let (_, fr) = split_descriptor_params(&f)?;
    let count = tp.len() as i64;
    let at = i64::from(pos);
    if at < 0 || (fr == "V" && at > count) || (fr != "V" && at >= count) {
        // `newIllegalArgumentException(msg, target, pos)`: the handle prints
        // as `MethodHandle` + its type.
        let shown = method_type_display_in(&*ctx, &t)?;
        return Some(combinator_iae(format!(
            "position is out of range for target: MethodHandle{shown}, {pos}"
        )));
    }
    if fr == "V" || tp.get(at as usize) == Some(&fr) {
        return None;
    }
    combinator_types_iae(&*ctx, "target and filter types do not match", &t, &f)
}

/// `guardWithTest(test, target, fallback)`: the three types, then target
/// against fallback, the predicate, and the test's parameters as a prefix of
/// the target's (`dropArgumentsToMatch`).
pub(crate) fn guard_with_test_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallFailed> {
    let (Some(test), Some(target), Some(fallback)) = (
        combinator_arg(args, 0),
        combinator_arg(args, 1),
        combinator_arg(args, 2),
    ) else {
        return combinator_first_null_handle(&[
            (combinator_arg(args, 0), "test"),
            (combinator_arg(args, 1), "target"),
            (combinator_arg(args, 2), "fallback"),
        ]);
    };
    let g = combinator_direct_type(ctx, test)?;
    let t = combinator_direct_type(ctx, target)?;
    let f = combinator_direct_type(ctx, fallback)?;
    if t != f {
        return combinator_mismatch_iae(&*ctx, "target and fallback types", &t, &f);
    }
    let (gp, gr) = split_descriptor_params(&g)?;
    if gr != "Z" {
        let shown = method_type_display_in(&*ctx, &g)?;
        return Some(combinator_iae(format!(
            "guard type is not a predicate {shown}"
        )));
    }
    let (tp, _) = split_descriptor_params(&t)?;
    if gp.len() > tp.len() || gp.iter().zip(&tp).any(|(a, b)| a != b) {
        return combinator_mismatch_iae(&*ctx, "target and test types", &t, &g);
    }
    None
}

/// The MethodType `catchException` judges a handle by (wave 99, lane L4):
/// [`combinator_direct_type`]'s answer, else the `type` field of any model
/// handle whose `type` is its adapted one ([`kind_has_authoritative_adapted_type`]:
/// a bound or `asType` copy of a direct handle keeps its kind and restamps
/// `type`; `insertArguments`, `dropArguments`, `filterReturnValue`,
/// `guardWithTest`, ... mint their adapted `type`, measured by
/// `MhCombinatorSweep` / `MhAdapterInvokeExactSweep`). Any other handle (a real
/// JDK handle with none of this model's slots, an invoker, a lambda factory)
/// stays unjudged: an unknown type is an accept.
pub(crate) fn catch_judged_type(ctx: &mut dyn NativeContext, mh: ObjectRef) -> Option<String> {
    if let Some(t) = combinator_direct_type(ctx, mh) {
        return Some(t);
    }
    if ctx.object_num_fields(mh) <= MH_BOUND {
        return None;
    }
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => return None,
    };
    if !kind_has_authoritative_adapted_type(kind) {
        return None;
    }
    mh_declared_descriptor(ctx, mh)
}

/// `catchException(target, exType, handler)`: the two types, then `exType`
/// a `Throwable` (`ClassCastException(exType.getName())`), the handler's
/// first parameter assignable from it, the return types, and the handler's
/// other parameters as a prefix of the target's.
pub(crate) fn catch_exception_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallFailed> {
    let (Some(target), Some(handler)) = (combinator_arg(args, 0), combinator_arg(args, 2)) else {
        return combinator_first_null_handle(&[
            (combinator_arg(args, 0), "target"),
            (combinator_arg(args, 2), "handler"),
        ]);
    };
    // A null `exType` is `Throwable.class.isAssignableFrom(null)`'s
    // message-less NPE on HotSpot too.
    let Some(ex_mirror) = combinator_arg(args, 1) else {
        return Some(combinator_npe());
    };
    // Interpreter round i4 wave 99, lane L4 (probe
    // `tools/probes/interp/L4/L4W99CatchExceptionAdapters.java`): every check
    // is asked in the JDK's order of the types it needs, and a handle is judged
    // by its MethodType whatever it is ([`catch_judged_type`]): a `bindTo`
    // result, an `asType` / `insertArguments` / `dropArguments` adapter. Only
    // plain direct handles were judged ([`combinator_direct_type`]), so a
    // return-type or leading-parameter mismatch over an adapter built a
    // `catchException` adapter that HotSpot refuses.
    let ex = mirror_class_id(&*ctx, ex_mirror)?;
    let ex_name = ctx.class_name_of_id(ex)?.class_text(&*ctx);
    let throwable = ctx.class_id_by_name("java/lang/Throwable")?;
    if ex != throwable && !ctx.is_subclass(ex, throwable) {
        // Wave 98 (lane L4, `L4W98CombinatorTexts catch-interface-type`): an
        // interface is never assignable to the class `Throwable`, so
        // `Throwable.class.isAssignableFrom(exType)` refuses it too, with the
        // same `ClassCastException(exType.getName())`; it was accepted here.
        return Some(RuntimeError::ClassCastException { message: ex_name }.into());
    }
    let h = catch_judged_type(ctx, handler)?;
    let (hp, hr) = split_descriptor_params(&h)?;
    let accepts = match hp.first() {
        None => false,
        Some(first) => match first.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            // `is_subclass` walks the interface DAG too, so an interface
            // first parameter (`Serializable`) is `isAssignableFrom`'s answer.
            Some(name) => {
                let param = ctx.class_id_by_name(name)?;
                param == ex || ctx.is_subclass(ex, param)
            }
            // A primitive or an array never accepts a `Throwable` class.
            None => false,
        },
    };
    if !accepts {
        return Some(combinator_iae(format!(
            "handler does not accept exception type class {ex_name}"
        )));
    }
    let t = catch_judged_type(ctx, target)?;
    let (tp, tr) = split_descriptor_params(&t)?;
    if hr != tr {
        return combinator_mismatch_iae(&*ctx, "target and handler return types", &t, &h);
    }
    let rest = &hp[1..];
    if rest.len() > tp.len() || rest.iter().zip(&tp).any(|(a, b)| a != b) {
        return combinator_mismatch_iae(&*ctx, "target and handler types", &t, &h);
    }
    None
}

/// `permuteArguments(target, newType, reorder...)`: `permuteArgumentChecks`.
pub(crate) fn permute_arguments_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallFailed> {
    let (Some(target), Some(new_type), Some(reorder)) = (
        combinator_arg(args, 0),
        combinator_arg(args, 1),
        combinator_arg(args, 2),
    ) else {
        // JDK 25: `reorder.clone()`, then `target.type()`, then
        // `newType.returnType()` (wave 141).
        return Some(if combinator_arg(args, 2).is_none() {
            combinator_npe_invoking("[I.clone()", "reorder")
        } else if combinator_arg(args, 0).is_none() {
            combinator_npe_handle("target")
        } else {
            combinator_npe_invoking("java.lang.invoke.MethodType.returnType()", "newType")
        });
    };
    let old = combinator_direct_type(ctx, target)?;
    let new = methodtype_to_descriptor(ctx, new_type)?;
    let (op, or) = split_descriptor_params(&old)?;
    let (np, nr) = split_descriptor_params(&new)?;
    if or != nr {
        return combinator_types_iae(&*ctx, "return types do not match", &old, &new);
    }
    let len = ctx.array_length(reorder);
    let mut indices: Vec<i32> = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(reorder, i) {
            Value::Int(v) => indices.push(v),
            _ => return None,
        }
    }
    if indices.len() != op.len() {
        let shown = method_type_display_in(&*ctx, &old)?;
        let listed: Vec<String> = indices.iter().map(i32::to_string).collect();
        return Some(combinator_iae(format!(
            "old type parameter count and reorder array length do not match: {shown}, [{}]",
            listed.join(", ")
        )));
    }
    for (j, &i) in indices.iter().enumerate() {
        if i < 0 || i as usize >= np.len() {
            let shown = method_type_display_in(&*ctx, &new)?;
            return Some(combinator_iae(format!(
                "index is out of bounds for new type: {i}, {shown}"
            )));
        }
        if np[i as usize] != op[j] {
            return combinator_types_iae(
                &*ctx,
                "parameter types do not match after reorder",
                &old,
                &new,
            );
        }
    }
    None
}
pub(crate) fn mhs_permute_arguments(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = target MH, args[1] = newType (MethodType), args[2] = int[] reorder
    if mh_combinator_checks_in_this_mode(&*ctx) {
        if let Some(refusal) = permute_arguments_refusal(ctx, args) {
            return Err(refusal);
        }
    }
    let target_mh = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_type = match args.get(1) {
        Some(Value::Object(Some(mt))) => *mt,
        _ => return Ok(Some(Value::Object(None))),
    };
    let reorder_arr = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Build the new descriptor from the MethodType
    let new_desc = descriptor_from_method_type(ctx, new_type);

    // Every element of `reorder` must be a nonnegative index less than
    // `newType.parameterCount()` — javadoc's own words. Measured
    // (`p.permuteBadIndex`, `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`'s
    // residual list): a 2-parameter `newType` with a `5` in the reorder array
    // was silently accepted where HotSpot 25 throws `IllegalArgumentException`
    // at THIS call, before any invocation — an out-of-range index would
    // otherwise read past the target's real argument list at dispatch time.
    if let Some((new_params, _)) = split_descriptor_params(&new_desc) {
        let n = new_params.len() as i32;
        let len = ctx.array_length(reorder_arr);
        for i in 0..len {
            if let Value::Int(idx) = ctx.get_array_element(reorder_arr, i) {
                if idx < 0 || idx >= n {
                    return Err(
                        cratonvm_types::error::RuntimeError::IllegalArgumentException {
                            message: format!("index out of bounds for {n} parameters: {idx}"),
                        }
                        .into(),
                    );
                }
            }
        }
    }

    // GC-safety: `alloc_concurrent_synthetic`/`alloc_method_handle` below
    // can trigger a collection that relocates `target_mh`/`reorder_arr`/
    // `wrapper` (all captured/produced above and read again afterward);
    // pin them and re-read the forwarded references before use.
    let target_pin = ctx.pin_native_root(target_mh);
    let reorder_pin = ctx.pin_native_root(reorder_arr);
    // Create a wrapper synthetic to hold (target_mh, reorder_arr)
    let wrapper = alloc_mh_carrier(ctx, "__mh_permute_wrapper__", 2);
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let target_mh = ctx.read_native_pin(target_pin, target_mh);
    let reorder_arr = ctx.read_native_pin(reorder_pin, reorder_arr);
    ctx.set_field(wrapper, 0, Value::Object(Some(target_mh)));
    ctx.set_field(wrapper, 1, Value::Object(Some(reorder_arr)));

    // Create the adapter MH with kind=PERMUTE
    let adapter: ObjectRef =
        alloc_method_handle(ctx, "__adapter__", "permute", &new_desc, MH_KIND_PERMUTE)?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    ctx.unpin_native_roots(target_pin);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

// ---------------------------------------------------------------------------
// T2.8.12 — MethodHandles.guardWithTest
// ---------------------------------------------------------------------------

pub(crate) fn mhs_guard_with_test(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = test MH, args[1] = target MH, args[2] = fallback MH
    if mh_combinator_checks_in_this_mode(&*ctx) {
        if let Some(refusal) = guard_with_test_refusal(ctx, args) {
            return Err(refusal);
        }
    }
    let test_mh = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target_mh = match args.get(1) {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let fallback_mh = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Build the GUARD adapter's descriptor from the target's EFFECTIVE
    // PARAMETERS but its RAW RETURN TYPE.
    //
    // Parameters: chain off the adapted MethodType (`mh_type_descriptor`) so the
    // adapter has the correct ARITY — it reflects the receiver-prepend of a
    // virtual/special target and any insertArguments/asCollector arity changes.
    // The raw `MH_DESC` (`mh_read_desc`) omits the receiver for an unbound
    // virtual target; using it dropped the receiver, so the GUARD adapter
    // reported one FEWER parameter than the target. Groovy's
    // `Selector.setGuards` reads `handle.type().parameterArray()` for the
    // `SAME_CLASSES` collector count while building `classes[]` from the
    // (longer) runtime args, so the shrunken count made `sameClasses(cs, os)`
    // index past `os` → AIOOBE in fromCache.
    //
    // Return type: keep the target's RAW return type, NOT the effective one.
    // CratonVM's `asType` is a passthrough that stamps the `type` field but
    // leaves `MH_DESC` carrying the LEAF method's real (possibly primitive or
    // void) return type. `auto_box_return` keys off that descriptor at the
    // signature-polymorphic invoke boundary to box a primitive return
    // (`Z`→Boolean, …) or map void→null — boxing the real JDK's `asType(…→
    // Object)` adapter would otherwise do. Taking the EFFECTIVE return type here
    // erased it to `Object` (`L`), which (a) dropped the `Z`→Boolean boxing so a
    // Groovy `version.endsWith("-SNAPSHOT")` came back null/false, and (b) turned
    // a void leaf's `Ok(None)` into an operand-stack underflow in fromCache.
    let eff = mh_type_descriptor(ctx, target_mh).unwrap_or_default();
    let raw = mh_read_desc(ctx, target_mh).unwrap_or_default();
    // Wave 58: the target's effective return, for the `type` below.
    let eff_ret = split_descriptor_params(&eff).map(|(_, ret)| ret);
    let target_desc = match (split_descriptor_params(&eff), split_descriptor_params(&raw)) {
        (Some((params, _eff_ret)), Some((_, raw_ret))) => {
            let mut d = String::with_capacity(eff.len());
            d.push('(');
            for p in &params {
                d.push_str(p);
            }
            d.push(')');
            d.push_str(&raw_ret);
            d
        }
        // Fall back to whichever descriptor parsed (effective preferred for arity).
        _ => {
            if !eff.is_empty() {
                eff
            } else {
                raw
            }
        }
    };

    // GC-safety: `alloc_concurrent_synthetic`/`alloc_method_handle` below
    // can trigger a collection that relocates `test_mh`/`target_mh`/
    // `fallback_mh`/`wrapper` (all captured/produced above and read again
    // afterward); pin them and re-read the forwarded references before use.
    let test_pin = ctx.pin_native_root(test_mh);
    let target_pin = ctx.pin_native_root(target_mh);
    let fallback_pin = ctx.pin_native_root(fallback_mh);
    // Create a wrapper synthetic to hold (test, target, fallback)
    let wrapper = alloc_mh_carrier(ctx, "__mh_guard_wrapper__", 3);
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let test_mh = ctx.read_native_pin(test_pin, test_mh);
    let target_mh = ctx.read_native_pin(target_pin, target_mh);
    let fallback_mh = ctx.read_native_pin(fallback_pin, fallback_mh);
    ctx.set_field(wrapper, 0, Value::Object(Some(test_mh)));
    ctx.set_field(wrapper, 1, Value::Object(Some(target_mh)));
    ctx.set_field(wrapper, 2, Value::Object(Some(fallback_mh)));

    // Create the adapter MH with kind=GUARD. `target_desc` already carries the
    // full (receiver-inclusive) param list, so alloc_method_handle installs a
    // `type` MethodType with the correct arity for the GUARD kind (which does
    // NOT itself prepend a receiver).
    //
    // Interpreter round i2 wave 58, lane L4 (`CRATONVM_MH_GUARD_TYPE_IS_TARGETS`):
    // the adapter's `type` -- what `type()`, `MutableCallSite.setTarget` and
    // `invokeExact` see -- is the TARGET's own `MethodType`, as on HotSpot,
    // when the target's return was retyped (`plus.asType((Object,Object)Object)`
    // over a `(long,long)long` leaf): it read `(Object,Object)long`, so
    // `setTarget` of such a guard refused with `MethodHandle(Object,Object)long
    // should be of type (Object,Object)Object` (`L4W58MixedWidthArguments`
    // `callsite-integer`). `MH_DESC` keeps the raw return the boxing keys on.
    let target_type = match ctx.get_field_by_name(target_mh, "type") {
        Value::Object(Some(mt))
            if crate::nbflags().mh_guard_type_is_targets
                && eff_ret.as_ref().is_some_and(|eff_ret| {
                    split_descriptor_params(&target_desc).is_some_and(|(_, ret)| ret != *eff_ret)
                }) =>
        {
            Some((ctx.pin_native_root(mt), mt))
        }
        _ => None,
    };
    let adapter: ObjectRef =
        alloc_method_handle(ctx, "__adapter__", "guard", &target_desc, MH_KIND_GUARD)?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    if let Some((type_pin, mt)) = target_type {
        let mt = ctx.read_native_pin(type_pin, mt);
        if crate::nbflags().dbg_mh_dispatch {
            eprintln!(
                "[MH_GUARD_TYPE] {target_desc} returns {}",
                eff_ret.as_deref().unwrap_or("?")
            );
        }
        ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
    }
    ctx.unpin_native_roots(test_pin);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

// ---------------------------------------------------------------------------
// T15 — java/lang/invoke/MethodHandleNatives
// ---------------------------------------------------------------------------

// JDK-ONLY-LAYOUT: `java.lang.invoke.MemberName`'s six declared instance
// fields, in declaration order. Confirmed with
// `javap -p --module java.base java.lang.invoke.MemberName` against Temurin
// 25.0.3 — not from memory, and not from the JDK-source comments that used to
// be copied into each native here (two of them disagreed with each other about
// slot 4).
//
//   0 clazz      Ljava/lang/Class;
//   1 name       Ljava/lang/String;
//   2 type       Ljava/lang/Object;                       (MethodType or Class)
//   3 flags      I
//   4 method     Ljava/lang/invoke/ResolvedMethodName;
//   5 resolution Ljava/lang/Object;                       (null == resolved)
//
// `vmindex` and `vmtarget` are `@Injected` in HotSpot: the class file declares
// NO field for either, so on a real layout there is no slot for them at all.
// That is what made slot 4 a defect — see `mn_set_vmindex`.
pub(crate) const MN_CLAZZ: usize = 0;
pub(crate) const MN_NAME: usize = 1;
pub(crate) const MN_TYPE: usize = 2;
pub(crate) const MN_FLAGS: usize = 3;
/// The synthetic model's "vmindex" slot. On a real `MemberName` this index is
/// `method`, a `ResolvedMethodName` REFERENCE — see [`mn_set_vmindex`].
pub(crate) const MN_VMINDEX: usize = 4;
pub(crate) const MN_RESOLUTION: usize = 5;
pub(crate) const MN_FIELD_COUNT: usize = 6;

/// JDK-ONLY-LAYOUT: does this `MemberName` have OUR fabricated layout, or is it
/// a real `java.lang.invoke.MemberName`?
///
/// Same shape, and for the same reason, as [`vh_has_synthetic_layout`]: ask for
/// a field NAME the real class declares and a VM-fabricated stub cannot have.
/// `ensure_synthetic_class` names fabricated slots `_f0.._fN` and types them
/// all `Ljava/lang/Object;`, so a real `MemberName` — and only a real one —
/// declares an instance field called `method`.
///
/// **Not a field count.** `alloc_concurrent_synthetic` returns an object with
/// at least the requested slot count either way, so `object_num_fields(mn) >=
/// MN_FIELD_COUNT` cannot tell the two layouts apart; that exact predicate was
/// written for `VarHandle` first, measured completely inert, and replaced by a
/// name test. See the doc comment on `vh_has_synthetic_layout`.
pub(crate) fn mn_has_synthetic_layout(ctx: &mut dyn NativeContext, mn: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(mn);
    !mhn_declared_fields_named(&*ctx, class_id, "method")
        .0
        .iter()
        .any(|f| !f.is_static && f.name == "method")
}

/// Resolve one of `MemberName`'s named fields on the RECEIVER's own class,
/// falling back to the fabricated model's slot index when the receiver does not
/// declare it.
///
/// The five named slots happen to sit at the same indices on both layouts
/// today, but "happen to" is what this whole work item is about: the sibling
/// defect in `native-collections` resolved `loadFactor` against a hard-coded
/// `java/util/HashMap` and wrote that index into whatever receiver it held.
/// Resolving on the receiver makes reads and writes agree by construction
/// instead of by coincidence.
pub(crate) fn mn_slot(ctx: &mut dyn NativeContext, mn: ObjectRef, name: &str, model: usize) -> usize {
    let class_id = ctx.class_id_of_object(mn);
    ctx.resolve_field_index_by_class_id(class_id, name)
        .unwrap_or(model)
}

pub(crate) fn mn_set(ctx: &mut dyn NativeContext, mn: ObjectRef, name: &str, model: usize, v: Value) {
    let slot = mn_slot(ctx, mn, name, model);
    ctx.set_field(mn, slot, v);
}

pub(crate) fn mn_get(ctx: &mut dyn NativeContext, mn: ObjectRef, name: &str, model: usize) -> Value {
    let slot = mn_slot(ctx, mn, name, model);
    ctx.get_field(mn, slot)
}

/// Publish the "resolved" vmindex sentinel — on OUR layout only.
///
/// JDK-ONLY-LAYOUT, kind 3 (VM-internal value with no real field at all), the
/// same kind as `CL_LOADER_ID` in `classloader.rs`. `vmindex` is `@Injected`:
/// HotSpot adds it to the object at VM level and the class file declares
/// nothing for it, so on a real `MemberName` there is no slot that means
/// vmindex. Index 4 there is `method`, a `ResolvedMethodName` reference, and
/// four natives were writing `Int(1)` into it — 7 hits per
/// `JdkOnlyBreadthProbe` run in the 2026-08-04 census, `Int` over `L`.
///
/// **The sentinel never survived that write.** `set_field` coerces by the
/// declared descriptor, and `coerce_field_value_by_descriptor` maps an `Int`
/// written to an `L` slot to `Object(None)` (gc/src/heap.rs) — which is
/// precisely the condition `overlay_write_is_destructive` reports. So on a real
/// image every one of those writes stored null, every `get_field(mn, 4)` read
/// back `Object(None)`, and both vmindex readers below already fell to their
/// `_ => 0` arm. Skipping the write on a real layout is therefore behaviour-
/// preserving for the readers, and it stops nulling `method`.
///
/// The comment this replaces claimed the non-zero sentinel was "critical"
/// because the JDK's `SplitConstantPool` throws
/// `ConstantPoolException("Bad CP index: 0")` on `entryByIndex(0)`. Whatever
/// that was true of, it cannot have been this write in real-JDK mode: the value
/// has never reached the object there.
pub(crate) fn mn_set_vmindex(ctx: &mut dyn NativeContext, mn: ObjectRef, vmindex: i32) {
    if mn_has_synthetic_layout(ctx, mn) {
        ctx.set_field(mn, MN_VMINDEX, Value::Int(vmindex));
    }
}

/// Read the vmindex sentinel back. Mirrors [`mn_set_vmindex`]: on a real layout
/// there is no vmindex slot, so the answer is 0 — which is what the raw slot-4
/// read returned there anyway, once the coercion had done its work.
pub(crate) fn mn_get_vmindex(ctx: &mut dyn NativeContext, mn: ObjectRef) -> i32 {
    if !mn_has_synthetic_layout(ctx, mn) {
        return 0;
    }
    match ctx.get_field(mn, MN_VMINDEX) {
        Value::Int(i) => i,
        _ => 0,
    }
}

/// `CRATONVM_MHN_RESOLVE_MEMO` -- performance, default OFF, `truthy_word`
/// (interpreter round i3 wave 73, lane L4; proposal
/// `i72-L4-proposal-mhn-resolve-memoises-its-member-answers-20261105`): the
/// field natives a `Lookup` field finder calls under `--jdk-only`
/// (`resolve` of a field `MemberName`, `objectFieldOffset`,
/// `staticFieldOffset`, `staticFieldBase`) answer a member they found before
/// from [`MHN_MEMO`] instead of re-deriving it.
///
/// Why a hit answers what the walk would: the key is the ADDRESSES of the
/// member's `clazz` mirror, `name` string and `type` mirror plus the requested
/// `flags`, in one collection epoch under one VM (nothing moves or is freed
/// within an epoch, so an equal address is the same object -- the argument of
/// [`REAL_HANDLE_MEMO`]); a class's declared fields, their flags and their
/// slots are fixed once it is loaded (redefinition may not change fields), and
/// a class unload happens only in a collection, which ends the epoch. Only a
/// FOUND member is stored (an absent member's error and the speculative
/// `null` are re-derived every time), and every write the natives make
/// (`MemberName.flags`, `clazz` for an inherited static, the static offset's
/// registration) is still made per call.
pub(crate) fn mhn_resolve_memo_enabled() -> bool {
    crate::nbflags().mhn_resolve_memo
}

/// The slots of a `MemberName` class's named fields and whether it is the
/// fabricated layout ([`mn_has_synthetic_layout`], [`mn_slot`]), decoded once
/// per class and epoch under [`mhn_resolve_memo_enabled`].
#[derive(Clone, Copy)]
pub(crate) struct MnLayout {
    pub(crate) synthetic: bool,
    pub(crate) clazz: usize,
    pub(crate) name: usize,
    pub(crate) ty: usize,
    pub(crate) flags: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct MnLayoutLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) class_id: ClassId,
    pub(crate) layout: MnLayout,
}

/// Which native a [`MhnMemoLine`] answers for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MhnMemoKind {
    Resolve = 1,
    ObjectOffset = 2,
    StaticOffset = 3,
    StaticBase = 4,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MhnMemoAnswer {
    /// `resolve` of a field member: the `flags` word it wrote (`None`: none),
    /// and the declaring class it wrote into `clazz` (an inherited static).
    Resolve {
        flags: Option<i32>,
        holder: Option<ClassId>,
    },
    /// `objectFieldOffset`: the instance slot.
    Offset(i64),
    /// `staticFieldOffset`: the offset and the static field it was registered for.
    StaticOffset {
        offset: i64,
        class_id: ClassId,
        field_index: usize,
    },
    /// `staticFieldBase`: the declaring class.
    StaticBase(ClassId),
}

#[derive(Clone, Copy)]
pub(crate) struct MhnMemoLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) kind: MhnMemoKind,
    pub(crate) key: [usize; 3],
    pub(crate) flags: i32,
    pub(crate) answer: MhnMemoAnswer,
}

pub(crate) const MHN_MEMO_SLOTS: usize = 64;

thread_local! {
    /// One remembered `MemberName` layout (there is one `MemberName` class per VM).
    pub(crate) static MN_LAYOUT_MEMO: std::cell::Cell<Option<MnLayoutLine>> =
        const { std::cell::Cell::new(None) };
    /// The memo of [`mhn_resolve_memo_enabled`], direct-mapped by key.
    pub(crate) static MHN_MEMO: std::cell::RefCell<Vec<Option<MhnMemoLine>>> =
        std::cell::RefCell::new(vec![None; MHN_MEMO_SLOTS]);
}

/// The layout of `mn`'s class: memoised per VM, class and epoch; a context of
/// VM identity `0` (the mocks) decodes every time.
pub(crate) fn mn_layout(ctx: &mut dyn NativeContext, mn: ObjectRef) -> MnLayout {
    let class_id = ctx.class_id_of_object(mn);
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    if vm != 0 {
        if let Some(line) = MN_LAYOUT_MEMO.with(|m| m.get()) {
            if line.vm == vm && line.epoch == epoch && line.class_id == class_id {
                return line.layout;
            }
        }
    }
    let layout = MnLayout {
        synthetic: mn_has_synthetic_layout(ctx, mn),
        clazz: mn_slot(ctx, mn, "clazz", MN_CLAZZ),
        name: mn_slot(ctx, mn, "name", MN_NAME),
        ty: mn_slot(ctx, mn, "type", MN_TYPE),
        flags: mn_slot(ctx, mn, "flags", MN_FLAGS),
    };
    if vm != 0 {
        MN_LAYOUT_MEMO.with(|m| {
            m.set(Some(MnLayoutLine {
                vm,
                epoch,
                class_id,
                layout,
            }))
        });
    }
    layout
}

pub(crate) fn mhn_memo_slot(kind: MhnMemoKind, key: &[usize; 3]) -> usize {
    let h = key[0] ^ key[1].rotate_left(17) ^ key[2].rotate_left(31) ^ ((kind as usize) << 3);
    (h >> 3) & (MHN_MEMO_SLOTS - 1)
}

pub(crate) fn mhn_memo_get(
    ctx: &dyn NativeContext,
    kind: MhnMemoKind,
    key: [usize; 3],
    flags: i32,
) -> Option<MhnMemoAnswer> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let slot = mhn_memo_slot(kind, &key);
    MHN_MEMO.with(|m| match m.borrow()[slot] {
        Some(line)
            if line.vm == vm
                && line.epoch == epoch
                && line.kind == kind
                && line.key == key
                && line.flags == flags =>
        {
            Some(line.answer)
        }
        _ => None,
    })
}

/// Store `answer` for a key read in collection epoch `epoch`; nothing is
/// stored when a collection ran since (the key's addresses may be stale).
pub(crate) fn mhn_memo_put(
    ctx: &dyn NativeContext,
    epoch: u64,
    kind: MhnMemoKind,
    key: [usize; 3],
    flags: i32,
    answer: MhnMemoAnswer,
) {
    let vm = ctx.vm_identity();
    if vm == 0 || ctx.gc_address_epoch() != epoch {
        return;
    }
    let slot = mhn_memo_slot(kind, &key);
    MHN_MEMO.with(|m| {
        m.borrow_mut()[slot] = Some(MhnMemoLine {
            vm,
            epoch,
            kind,
            key,
            flags,
            answer,
        });
    });
}

pub(crate) fn mhn_obj_addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// A field `MemberName`'s resolve key under [`mhn_resolve_memo_enabled`]:
/// `None` for the fabricated layout, a member that is not a field kind, or a
/// slot that does not hold what a finder writes.
pub(crate) struct MhnResolveKey {
    pub(crate) epoch: u64,
    pub(crate) key: [usize; 3],
    pub(crate) flags: i32,
    pub(crate) layout: MnLayout,
}

pub(crate) fn mhn_resolve_memo_key(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<MhnResolveKey> {
    let layout = mn_layout(ctx, mn);
    if layout.synthetic {
        return None;
    }
    let Value::Int(flags) = ctx.get_field(mn, layout.flags) else {
        return None;
    };
    if !(1..=4).contains(&((flags >> 24) & 0x0F)) {
        return None;
    }
    let Value::Object(Some(clazz)) = ctx.get_field(mn, layout.clazz) else {
        return None;
    };
    let Value::Object(Some(name)) = ctx.get_field(mn, layout.name) else {
        return None;
    };
    let Value::Object(Some(ty)) = ctx.get_field(mn, layout.ty) else {
        return None;
    };
    Some(MhnResolveKey {
        epoch: ctx.gc_address_epoch(),
        key: [mhn_obj_addr(clazz), mhn_obj_addr(name), mhn_obj_addr(ty)],
        flags,
        layout,
    })
}

/// Replay a remembered field resolve on `mn`: the same writes, in the same
/// order, as [`native_mhn_resolve`] made when it found the member.
pub(crate) fn mhn_resolve_memo_replay(
    ctx: &mut dyn NativeContext,
    mn: ObjectRef,
    layout: MnLayout,
    flags: Option<i32>,
    holder: Option<ClassId>,
) -> MethodCallResult {
    let mut mn = mn;
    if let Some(holder) = holder {
        // A mirror allocation, a GC point: `mn` is re-read through its pin.
        let pin = ctx.pin_native_root(mn);
        let holder_mirror = ctx.get_class_mirror(holder);
        mn = ctx.read_native_pin(pin, mn);
        ctx.unpin_native_roots(pin);
        ctx.set_field(mn, layout.clazz, Value::Object(Some(holder_mirror)));
    }
    if let Some(flags) = flags {
        ctx.set_field(mn, layout.flags, Value::Int(flags));
    }
    Ok(Some(Value::Object(Some(mn))))
}

/// `CRATONVM_MHN_RESOLVE_METHOD_MEMO` -- performance, default OFF,
/// `truthy_word` (interpreter round i3 wave 81, lane L4; proposal
/// `i73-L4-proposal-method-member-resolve-memo-and-mirror-judged-load-20261106`,
/// item 2): `MethodHandleNatives.resolve` of a METHOD `MemberName` (ref kinds
/// 5-9: every `findVirtual` / `findStatic` / `findSpecial` /
/// `findConstructor` / `unreflect*` under `--jdk-only`, and the JDK's own
/// `NamedFunction` resolution) answers a member this thread found before
/// from [`MHN_METHOD_MEMO`] instead of re-deriving it (two descriptor
/// renders, `method_exists`, the superinterface walk, the verdict walk, up to
/// three `declared_methods` clones and the caller access check).
///
/// Why a hit answers what the walk would, extending
/// [`mhn_resolve_memo_enabled`]'s argument:
///
/// * the key is the ADDRESSES of `clazz`, `name` and `type` (a `MethodType`
///   only: interned and immutable; an `Object[]` or a `String` form is never
///   a key), plus the requested `flags`, in one collection epoch under one VM;
/// * the access check depends on the CALLER and the LOOKUP MODE, so both are
///   in the key (the caller as its mirror's address, `0` for `null`), and the
///   speculative bit too (an unproven member answers `null` speculatively but
///   resolves as-is otherwise);
/// * a class's declared methods and their flags are fixed once it is loaded
///   (redefinition may not add, remove or re-flag a method), and a class
///   unload happens only in a collection, which ends the epoch; access can
///   only GROW at run time (`Module.addReads` / `addExports`), so a member
///   once admitted stays admitted;
/// * only a FOUND member (`method_exists`, or the verdict's `Found`) whose
///   class came from its mirror is stored; an absent member's error, the
///   speculative `null` and every refusal are re-derived each time; the one
///   write the method branch makes on a real layout (the merged `flags`) is
///   replayed per call.
pub(crate) fn mhn_resolve_method_memo_enabled() -> bool {
    crate::nbflags().mhn_resolve_method_memo
}

/// The key of a method `MemberName`'s resolve under
/// [`mhn_resolve_method_memo_enabled`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MhnMethodKey {
    /// The addresses of `clazz`, `name` and `type`.
    pub(crate) objects: [usize; 3],
    /// The requested `flags` (before resolution merges the `ACC_*` half).
    pub(crate) flags: i32,
    /// The `caller` argument's mirror address, `0` for `null`.
    pub(crate) caller: usize,
    /// The `lookupMode` argument.
    pub(crate) mode: i32,
    /// The `speculativeResolve` argument.
    pub(crate) speculative: bool,
}

/// [`MhnMethodKey`] plus what the replay needs.
pub(crate) struct MhnMethodMemoKey {
    pub(crate) epoch: u64,
    pub(crate) key: MhnMethodKey,
    pub(crate) flags_slot: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct MhnMethodMemoLine {
    pub(crate) vm: usize,
    pub(crate) epoch: u64,
    pub(crate) key: MhnMethodKey,
    /// The `flags` word the found member's resolve wrote (`None`: none).
    pub(crate) flags_written: Option<i32>,
}

pub(crate) const MHN_METHOD_MEMO_SLOTS: usize = 128;

thread_local! {
    /// The memo of [`mhn_resolve_method_memo_enabled`], direct-mapped by key.
    pub(crate) static MHN_METHOD_MEMO: std::cell::RefCell<Vec<Option<MhnMethodMemoLine>>> =
        std::cell::RefCell::new(vec![None; MHN_METHOD_MEMO_SLOTS]);
}

pub(crate) fn mhn_method_memo_slot(key: &MhnMethodKey) -> usize {
    let h = key.objects[0]
        ^ key.objects[1].rotate_left(13)
        ^ key.objects[2].rotate_left(29)
        ^ key.caller.rotate_left(41)
        ^ (key.flags as u32 as usize).rotate_left(7)
        ^ (key.mode as u32 as usize).rotate_left(53)
        ^ usize::from(key.speculative);
    ((h ^ (h >> 17)) >> 3) & (MHN_METHOD_MEMO_SLOTS - 1)
}

pub(crate) fn mhn_method_memo_get(ctx: &dyn NativeContext, key: &MhnMethodKey) -> Option<Option<i32>> {
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let epoch = ctx.gc_address_epoch();
    let slot = mhn_method_memo_slot(key);
    MHN_METHOD_MEMO.with(|m| match m.borrow()[slot] {
        Some(line) if line.vm == vm && line.epoch == epoch && line.key == *key => {
            Some(line.flags_written)
        }
        _ => None,
    })
}

/// Store a found member's answer for a key read in collection epoch `epoch`;
/// nothing is stored when a collection ran since (the key's addresses may be
/// stale) or for VM identity `0` (the mocks).
pub(crate) fn mhn_method_memo_put(
    ctx: &dyn NativeContext,
    epoch: u64,
    key: MhnMethodKey,
    flags_written: Option<i32>,
) {
    let vm = ctx.vm_identity();
    if vm == 0 || ctx.gc_address_epoch() != epoch {
        return;
    }
    let slot = mhn_method_memo_slot(&key);
    MHN_METHOD_MEMO.with(|m| {
        m.borrow_mut()[slot] = Some(MhnMethodMemoLine {
            vm,
            epoch,
            key,
            flags_written,
        });
    });
}

/// The method key of `mn` (the `resolve` call's `args`): `None` for the
/// fabricated layout, a member that is not a method kind, a `null` slot, or a
/// `type` that is not a `MethodType`. Reads only.
#[inline(never)]
pub(crate) fn mhn_resolve_method_memo_key(
    ctx: &mut dyn NativeContext,
    mn: ObjectRef,
    args: &[Value],
) -> Option<MhnMethodMemoKey> {
    let layout = mn_layout(ctx, mn);
    if layout.synthetic {
        return None;
    }
    let Value::Int(flags) = ctx.get_field(mn, layout.flags) else {
        return None;
    };
    if !(5..=9).contains(&((flags >> 24) & 0x0F)) {
        return None;
    }
    let Value::Object(Some(clazz)) = ctx.get_field(mn, layout.clazz) else {
        return None;
    };
    let Value::Object(Some(name)) = ctx.get_field(mn, layout.name) else {
        return None;
    };
    let Value::Object(Some(ty)) = ctx.get_field(mn, layout.ty) else {
        return None;
    };
    if ctx.class_name_of_id(ctx.class_id_of_object(ty)).as_deref()
        != Some("java/lang/invoke/MethodType")
    {
        return None;
    }
    let caller = match args.get(1) {
        Some(Value::Object(Some(m))) => mhn_obj_addr(*m),
        _ => 0,
    };
    let mode = match args.get(2) {
        Some(Value::Int(m)) => *m,
        _ => 0,
    };
    Some(MhnMethodMemoKey {
        epoch: ctx.gc_address_epoch(),
        key: MhnMethodKey {
            objects: [mhn_obj_addr(clazz), mhn_obj_addr(name), mhn_obj_addr(ty)],
            flags,
            caller,
            mode,
            speculative: matches!(args.get(3), Some(Value::Int(1))),
        },
        flags_slot: layout.flags,
    })
}

/// `MemberName.TRUSTED_FINAL` (`MethodHandleNatives.Constants.MN_TRUSTED_FINAL`):
/// HotSpot's `init_field_MemberName` sets it for a `final` field that is
/// static, or of a hidden class or a record (`fieldDescriptor::is_trusted_final`).
pub(crate) const MN_TRUSTED_FINAL: i32 = 0x0020_0000;

/// The flags HotSpot's `init_field_MemberName` writes for a resolved field:
/// the request's flags with the reference kind `ref_kind` and the field's
/// `access` (`JVM_RECOGNIZED_FIELD_MODIFIERS`), plus [`MN_TRUSTED_FINAL`] for
/// a final field of a hidden class (`hidden_holder`). Pure.
pub(crate) fn mhn_hidden_field_flags(request: i32, ref_kind: i32, access: u16, hidden_holder: bool) -> i32 {
    let mut flags =
        (request & !(0x0F << 24) & !0xFFFF) | (ref_kind << 24) | (i32::from(access) & 0xFFFF);
    if hidden_holder && access & 0x0010 != 0 {
        flags |= MN_TRUSTED_FINAL;
    }
    flags
}

/// The reference kind HotSpot's field resolution settles on for a request of
/// `ref_kind` (1..=4) that found a field whose static-ness is `is_static`: a
/// get/put of the other static-ness is rewritten to the field's own, which
/// `Lookup.checkField` then refuses as "expected a (non-)static field".
pub(crate) fn mhn_field_ref_kind(ref_kind: i32, is_static: bool) -> i32 {
    match (ref_kind, is_static) {
        (1, true) | (3, true) => ref_kind + 1,
        (2, false) | (4, false) => ref_kind - 1,
        _ => ref_kind,
    }
}

/// `MethodHandleNatives.resolve` of a FIELD `MemberName` whose `clazz` is a
/// lambda proxy (interpreter round i3 wave 80, lane L4; page
/// `i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack`,
/// `--jdk-only` half). A proxy's mirror names no class, so
/// [`native_mhn_resolve`] returned such a member as it came: without the
/// capture's `ACC_PRIVATE | ACC_FINAL`, so `VarHandles.makeFieldHandle` built a
/// read-WRITE handle and `findSetter` of the final capture was admitted, and
/// without the by-name-and-type check, so an absent capture or a wrong type
/// got a handle. Resolved here as HotSpot does: the capture found by name and
/// type ([`crate::lang_class::declared_fields_with_aliases`]), its flags merged
/// with `TRUSTED_FINAL` (a hidden class's final field); none found is
/// `NoSuchFieldError` (the JDK's `NoSuchFieldException: no such field:
/// P.f/T/getField`), or `null` for a speculative resolve. `None` for anything
/// that is not a field member of a lambda proxy on the JDK's own layout (the
/// caller returns the member as before). Reads only: no GC point.
#[cold]
#[inline(never)]
pub(crate) fn mhn_resolve_lambda_proxy_field(
    ctx: &mut dyn NativeContext,
    member_name: ObjectRef,
    class_mirror: ObjectRef,
    speculative: bool,
) -> Option<MethodCallResult> {
    let (proxy_id, proxy_name) = lk_lambda_proxy_of_mirror(&*ctx, class_mirror)?;
    let flags = match mn_get(ctx, member_name, "flags", MN_FLAGS) {
        Value::Int(f) => f,
        _ => return None,
    };
    let ref_kind = (flags >> 24) & 0x0F;
    if !(1..=4).contains(&ref_kind) || mn_has_synthetic_layout(ctx, member_name) {
        return None;
    }
    let name = match mn_get(ctx, member_name, "name", MN_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let field_desc = match mn_get(ctx, member_name, "type", MN_TYPE) {
        Value::Object(Some(ty))
            if ctx.class_name_of_id(ctx.class_id_of_object(ty)).as_deref()
                == Some("java/lang/Class") =>
        {
            field_descriptor_from_mirror(&*ctx, ty).into_owned()
        }
        _ => return None,
    };
    let field = crate::lang_class::declared_fields_with_aliases(&*ctx, proxy_id)
        .into_iter()
        .find(|f| f.name == name && f.descriptor == field_desc);
    let Some(field) = field else {
        if speculative {
            return Some(Ok(Some(Value::Object(None))));
        }
        return Some(Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoSuchFieldError {
                class_name: proxy_name.replace('.', "/"),
                field_name: name,
                field_descriptor: field_desc,
            },
        ))));
    };
    let resolved_ref_kind = mhn_field_ref_kind(ref_kind, field.is_static);
    let merged = mhn_hidden_field_flags(flags, resolved_ref_kind, field.access_flags, true);
    if merged != flags {
        mn_set(ctx, member_name, "flags", MN_FLAGS, Value::Int(merged));
    }
    Some(Ok(Some(Value::Object(Some(member_name)))))
}

/// `MethodHandleNatives.resolve` of a METHOD `MemberName` (reference kinds
/// 5, 6, 7, 9) whose `clazz` is a lambda proxy (interpreter round i4 wave 85,
/// lane L4; `L4W61LambdaProxyMemberHandles` rows `findVirtual-absent`,
/// `findStatic-instance`, `findVirtual-wrong-type`). Since the JDK's finder
/// bodies run in both modes, every `Lookup.find*` on a proxy reaches this
/// native, which handed the member back as it came: a handle over a method
/// the proxy does not have (`findVirtual(P, "nope", ()void)`), or of a wrong
/// type, and `findStatic` of an instance method failed only in the Java-side
/// `checkMethod` ("expected a static method"). HotSpot's `MHN_resolve_Mem`
/// raises `NoSuchMethodError` for an absent member and
/// `IncompatibleClassChangeError` for one of the other static-ness;
/// `MemberName.makeAccessException` turns them into `NoSuchMethodException` /
/// `IllegalAccessException: no such method: P.m()R/kind`. A speculative
/// resolve answers `null` for both. The lookup is
/// [`lk_lambda_proxy_method_access`] (the proxy's own methods, `Object`'s,
/// its interfaces'), shared with the `--compatible` finders. `None` (the
/// caller answers the member as before) for a FOUND member, a constructor
/// kind, the fabricated layout, or a `type` that is not a `MethodType`.
/// Reads only: no GC point.
#[cold]
#[inline(never)]
pub(crate) fn mhn_resolve_lambda_proxy_method(
    ctx: &mut dyn NativeContext,
    member_name: ObjectRef,
    class_mirror: ObjectRef,
    speculative: bool,
) -> Option<MethodCallResult> {
    let (proxy_id, proxy_name) = lk_lambda_proxy_of_mirror(&*ctx, class_mirror)?;
    let flags = match mn_get(ctx, member_name, "flags", MN_FLAGS) {
        Value::Int(f) => f,
        _ => return None,
    };
    let ref_kind = (flags >> 24) & 0x0F;
    if !matches!(ref_kind, 5 | 6 | 7 | 9) || mn_has_synthetic_layout(ctx, member_name) {
        return None;
    }
    let name = match mn_get(ctx, member_name, "name", MN_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let desc = match mn_get(ctx, member_name, "type", MN_TYPE) {
        Value::Object(Some(mt))
            if ctx.class_name_of_id(ctx.class_id_of_object(mt)).as_deref()
                == Some("java/lang/invoke/MethodType") =>
        {
            descriptor_from_method_type(&*ctx, mt)
        }
        _ => return None,
    };
    if desc.is_empty() {
        return None;
    }
    let class_name = proxy_name.replace('.', "/");
    let error = match lk_lambda_proxy_method_access(&*ctx, proxy_id, &name, &desc) {
        None => LinkageError::NoSuchMethodError {
            class_name,
            method_name: name,
            method_descriptor: desc,
        },
        Some(access) => {
            let is_static = access & MHN_ACC_STATIC != 0;
            if is_static == (ref_kind == 6) {
                return None;
            }
            // Wave 137 (lane L4, `CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS`):
            // `LinkResolver`'s own text (a proxy is a class, never an
            // interface reference), naming the proxy as its external name.
            let message = if mhn_resolve_special_link_checks_enabled() {
                let method = mhn_external_method_name(&proxy_name, &name, &desc)
                    .unwrap_or_else(|| format!("{proxy_name}.{name}{desc}"));
                if ref_kind == 6 {
                    format!("Expected static method '{method}'")
                } else {
                    format!("Expecting non-static method '{method}'")
                }
            } else {
                format!(
                    "Expected {} method {}.{}{}",
                    if is_static {
                        "instance not static"
                    } else {
                        "static"
                    },
                    proxy_name,
                    name,
                    desc
                )
            };
            LinkageError::IncompatibleClassChangeError { message }
        }
    };
    if speculative {
        return Some(Ok(Some(Value::Object(None))));
    }
    Some(Err(MethodCallFailed::InternalError(VmError::Linkage(
        error,
    ))))
}

/// Is the `java.lang.reflect.Field` `field` (modifiers `modifiers`) a TRUSTED
/// final, the bit HotSpot's `init_field_MemberName` copies into the
/// `MemberName` it initializes (`fieldDescriptor::is_trusted_final`: final,
/// and static or of a hidden class or a record)? The `Field`'s own
/// `trustedFinal` (filled from the same predicate when it was built), or a
/// final capture of a lambda proxy -- a hidden class on HotSpot, but no
/// class-store class here, so `is_class_hidden` does not see it (interpreter
/// round i3 wave 80, lane L4; `L4W61LambdaProxyFieldHandles` row
/// `unreflectSetter-accessible`, `--jdk-only`: `MemberName(Field)` had no
/// `TRUSTED_FINAL`, so `Lookup.unreflectField` answered a setter of the final
/// capture where HotSpot raises `final field has no write access`). Reads only.
pub(crate) fn mhn_init_field_trusted_final(
    ctx: &mut dyn NativeContext,
    field: ObjectRef,
    modifiers: i32,
) -> bool {
    if modifiers & 0x0010 == 0 {
        return false;
    }
    if matches!(ctx.get_field_by_name(field, "trustedFinal"), Value::Int(v) if v != 0) {
        return true;
    }
    match ctx.get_field_by_name(field, "clazz") {
        Value::Object(Some(m)) => lk_lambda_proxy_of_mirror(&*ctx, m).is_some(),
        _ => false,
    }
}

/// The heap slot of the captured field a real `MemberName` names when its
/// `clazz` is a lambda proxy ([`native_mhn_object_field_offset`]; wave 80,
/// lane L4). `mhn_static_member` needs a class-store name, which a proxy does
/// not have, so the offset fell to the vmindex fallback `0` -- the first
/// capture, whatever the field. Reads only.
#[cold]
#[inline(never)]
pub(crate) fn mhn_lambda_proxy_field_slot(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<usize> {
    let Value::Object(Some(mirror)) = mn_get(ctx, mn, "clazz", MN_CLAZZ) else {
        return None;
    };
    let (proxy_id, _) = lk_lambda_proxy_of_mirror(&*ctx, mirror)?;
    let Value::Object(Some(name)) = mn_get(ctx, mn, "name", MN_NAME) else {
        return None;
    };
    let name = ctx.read_string(name)?;
    crate::lang_class::declared_fields_with_aliases(&*ctx, proxy_id)
        .into_iter()
        .find(|f| !f.is_static && f.name == name)
        .map(|f| f.slot_index)
}

/// `MethodHandleNatives.resolve(MemberName self, Class<?> caller, int lookupMode, boolean speculativeResolve)`
///
/// Resolves a MemberName object by looking up the referenced class/method/field.
/// In HotSpot this does full JVM-level resolution. Our implementation reads the
/// MemberName fields (clazz, name, type) and creates a resolved MemberName with
/// the vmindex and vmtarget fields populated.
///
/// For the layout — and for why the vmindex sentinel is no longer written into
/// slot 4 — see the `MN_*` constants and [`mn_set_vmindex`] above.
pub(crate) fn native_mhn_resolve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `CRATONVM_DBG=mh-finder-census` (interpreter round i4 wave 89, lane L4):
    // every call timed, and the share made inside a `Lookup` finder body.
    let Some(start) = crate::mh_finder_census::phase_start() else {
        return native_mhn_resolve_uncensused(ctx, args);
    };
    let in_body = crate::mh_finder_census::in_finder_body();
    let answer = native_mhn_resolve_uncensused(ctx, args);
    if in_body {
        // Wave 94: this body's resolve share (`body-dist` rows).
        crate::mh_finder_census::note_resolve_in_body(start.elapsed().as_nanos() as u64);
        crate::mh_finder_census::phase_end(
            crate::mh_finder_census::Phase::ResolveInFinderBody,
            Some(start),
        );
    }
    crate::mh_finder_census::phase_end(crate::mh_finder_census::Phase::Resolve, Some(start));
    if answer.is_err() {
        crate::mh_finder_census::event("mhn-resolve.threw");
    }
    answer
}

/// [`native_mhn_resolve`] proper (the census wrapper above times it).
/// `MethodHandleNatives.Constants.MN_CALLER_SENSITIVE`: "@CallerSensitive
/// annotation detected" (JDK 25).
pub(crate) const MN_CALLER_SENSITIVE: i32 = 0x0010_0000;

/// Is the method `name` (`descriptor`: exactly that overload; `None`: any of
/// the name) declared by `holder` HotSpot's `Method::caller_sensitive()`? The
/// class file parser honours `@jdk.internal.reflect.CallerSensitive` only in a
/// privileged class (`ClassFileParser`'s `AnnotationCollector`: the boot or
/// platform loader), and `init_method_MemberName` then sets
/// [`MN_CALLER_SENSITIVE`] in the member's `flags`, which
/// `MethodHandleNatives.isCallerSensitive` reads: `Lookup.maybeBindCaller`
/// binds such a method to the lookup class's injected invoker
/// (`MethodHandleImpl.BindCaller`) and `findBoundCallerLookup` refuses it to a
/// restricted lookup. This VM never set the bit, so a handle from
/// `findVirtual(Field.class, "get", ..)` called `Field.get` with the lookup
/// class as its caller, where HotSpot names `<Lookup>$$InjectedInvoker/0x..`
/// (`L3W94CallerSensitiveMatrix` row `get-other-handle`, both modes).
/// Interpreter round i4 wave 95, lane L4; `CRATONVM_MHN_CALLER_SENSITIVE_FLAG`
/// (default on, `=0` off). Reads only (one class-manager read in the VM, for a
/// holder of the boot or platform loader only; wave 96 made the stack walker's
/// `class_may_be_caller_sensitive` use the same loader rule).
pub(crate) fn mhn_method_is_caller_sensitive(
    ctx: &dyn NativeContext,
    holder: ClassId,
    name: &str,
    descriptor: Option<&str>,
) -> bool {
    if !crate::nbflags().mhn_caller_sensitive_flag || name.is_empty() {
        return false;
    }
    if !matches!(ctx.loader_id_of_class_definite(holder), Some(0 | 1)) {
        return false;
    }
    let Some(class_name) = ctx.class_name_arc_of_id(holder) else {
        return false;
    };
    let entry = cratonvm_native_api::StackTraceEntry {
        class_name,
        method_name: std::sync::Arc::from(name),
        method_descriptor: descriptor.map(std::sync::Arc::from),
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: Some(holder),
        method_index: None,
    };
    ctx.caller_sensitive_stack_entries(std::slice::from_ref(&entry))
        .first()
        .copied()
        .unwrap_or(false)
}

pub(crate) fn native_mhn_resolve_uncensused(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = MemberName self, args[1] = caller Class, args[2] = lookupMode, args[3] = speculativeResolve
    let member_name = crate::obj_arg(args, 0)?;

    // Interpreter round i3 wave 73, lane L4 (`CRATONVM_MHN_RESOLVE_MEMO`): a
    // field member this thread found before, in this epoch, replays its writes.
    let memo_key = if mhn_resolve_memo_enabled()
        && MHN_RESOLVE_LOAD_ONLY
        && MHN_RESOLVE_REPORTS_ABSENT_MEMBERS
    {
        mhn_resolve_memo_key(ctx, member_name)
    } else {
        None
    };
    if let Some(k) = &memo_key {
        if let Some(MhnMemoAnswer::Resolve { flags, holder }) =
            mhn_memo_get(&*ctx, MhnMemoKind::Resolve, k.key, k.flags)
        {
            crate::mh_finder_census::event("mhn-resolve.field-memo.hit");
            return mhn_resolve_memo_replay(ctx, member_name, k.layout, flags, holder);
        }
        crate::mh_finder_census::event("mhn-resolve.field-memo.miss");
    }
    // Interpreter round i3 wave 81, lane L4 (`CRATONVM_MHN_RESOLVE_METHOD_MEMO`):
    // a METHOD member this thread found before, in this epoch, for the same
    // caller, lookup mode and speculative bit, replays its `flags` write.
    let method_memo_key = if memo_key.is_none()
        && mhn_resolve_method_memo_enabled()
        && MHN_RESOLVE_LOAD_ONLY
        && MHN_RESOLVE_REPORTS_ABSENT_MEMBERS
    {
        mhn_resolve_method_memo_key(ctx, member_name, args)
    } else {
        None
    };
    if let Some(k) = &method_memo_key {
        if let Some(flags_written) = mhn_method_memo_get(&*ctx, &k.key) {
            crate::mh_finder_census::event("mhn-resolve.method-memo.hit");
            if let Some(merged) = flags_written {
                ctx.set_field(member_name, k.flags_slot, Value::Int(merged));
            }
            return Ok(Some(Value::Object(Some(member_name))));
        }
        crate::mh_finder_census::event("mhn-resolve.method-memo.miss");
    } else if memo_key.is_none() {
        crate::mh_finder_census::event("mhn-resolve.no-memo-key");
    }
    // Whether a method member was positively found (`method_exists`, or the
    // wave-23 verdict's `Found`), for the method memo's fill below.
    let mut method_found = false;
    // What a found field member wrote, for the memo's fill below.
    let mut field_found = false;
    let mut holder_written: Option<ClassId> = None;
    let mut flags_written: Option<i32> = None;
    // A found method's declaring class when it is not the named one, and the
    // reference kind it resolves to (wave 83; see the method branch).
    let mut method_holder: Option<(ClassId, i32)> = None;
    // Wave 95 (lane L4): the found method is `@CallerSensitive` in a JDK class
    // (`mhn_method_is_caller_sensitive`), so `flags` gains `MN_CALLER_SENSITIVE`.
    let mut caller_sensitive = false;
    // Interpreter round i5 wave 108, lane L3: the found virtual method is a
    // direct call (`mhn_virtual_is_direct_call`), so the settled kind is
    // `REF_invokeSpecial`, as HotSpot's `init_method_MemberName` writes it.
    let mut direct_call = false;

    let class_mirror = match mn_get(ctx, member_name, "clazz", MN_CLAZZ) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Object(Some(member_name)))), // no class → return as-is
    };

    let class_name = match crate::lang_class::mirror_class_name(ctx, class_mirror) {
        Some(n) => n,
        None => {
            // A lambda proxy's mirror names no class (wave 80, lane L4): its
            // captured fields resolve here, as HotSpot's `init_field_MemberName`.
            let speculative = matches!(args.get(3), Some(Value::Int(1)));
            if let Some(answer) =
                mhn_resolve_lambda_proxy_field(ctx, member_name, class_mirror, speculative)
            {
                return answer;
            }
            // Interpreter round i4 wave 85, lane L4: and its METHOD members
            // are refused as HotSpot refuses them (absent, or of the other
            // static-ness); a found one is answered as before.
            if let Some(answer) =
                mhn_resolve_lambda_proxy_method(ctx, member_name, class_mirror, speculative)
            {
                return answer;
            }
            return Ok(Some(Value::Object(Some(member_name))));
        }
    };
    // The class's identity, read from its mirror before anything below can
    // move the mirror: the wave-23 member checks search THIS class, not
    // whichever class a by-name load answers.
    let mirror_cid = mirror_class_id(&*ctx, class_mirror);
    // The `caller` argument's class, read here for the same reason (SBR3 mh).
    let caller_cid_early = match args.get(1) {
        Some(Value::Object(Some(m))) => mirror_class_id(&*ctx, *m),
        _ => None,
    };

    let name = match mn_get(ctx, member_name, "name", MN_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };

    // What kind of member this is.
    let flags = match mn_get(ctx, member_name, "flags", MN_FLAGS) {
        Value::Int(f) => f,
        _ => 0,
    };

    // Reference kind is encoded in bits 24-27 of flags
    let ref_kind = (flags >> 24) & 0x0F;

    // GC-safety: `ensure_class_initialized` below can trigger a collection
    // that relocates `member_name` (captured above, read/written again
    // afterward); pin it and re-read the forwarded reference before use.
    let member_name_pin = ctx.pin_native_root(member_name);
    // Ensure the class is loaded. HotSpot's `MHN_resolve_Mem` initializes
    // nothing; the handle built from the MemberName does, at its first call.
    //
    // A mirror that names its class answers by itself (interpreter round i3
    // wave 79, lane L4; pages
    // `i73-L4-mhn-resolve-judges-a-held-mirror-by-a-by-name-load` and
    // `i65-orch-jdk-only-findgetter-on-a-renamed-public-nested-class-is-package-private`):
    // the class is loaded by construction, and HotSpot resolves in the class
    // the `MemberName` holds. The by-name load resolved `class_name` from the
    // native caller's loader instead: for a class only a child loader defines
    // (`L4W65BoundFieldOwner`'s two renamed `BoxN`s) it found nothing, so the
    // `ACC_*` half of `flags` was never merged, `v` read as package-private,
    // and `Lookup.checkAccess` refused `findGetter` with "member is private
    // to package"; for a name two loaders define it could read the OTHER
    // class's modifiers. Only a mirror without a class id still loads by name.
    let resolved_class_id = match mirror_cid {
        Some(cid) => Some(cid),
        None if MHN_RESOLVE_LOAD_ONLY => lookup_load_class(ctx, &class_name).ok().flatten(),
        None => ctx.ensure_class_initialized(&class_name).ok(),
    };
    let member_name = ctx.read_native_pin(member_name_pin, member_name);

    // Whether this is our fabricated layout, asked ONCE (wave 73, lane L4: it
    // was asked twice per call, each a `declared_fields` clone of the
    // `MemberName` class; a pure refactor -- the receiver's class is fixed).
    let synthetic_layout = match &memo_key {
        Some(k) => k.layout.synthetic,
        None => mn_has_synthetic_layout(ctx, member_name),
    };
    // Mark as resolved with the non-zero vmindex sentinel — on our fabricated
    // layout only. See `mn_set_vmindex`: a real `MemberName` has no vmindex
    // field, and slot 4 there is `method`.
    if synthetic_layout {
        ctx.set_field(member_name, MN_VMINDEX, Value::Int(1));
    }

    // The `ACC_*` half of `flags`, which resolution is what fills in.
    //
    // `MemberName.flags` is `refKind<<24 | IS_METHOD/IS_FIELD/IS_CONSTRUCTOR |
    // ACC_*`. The Java-side constructors set the kind and the reference kind
    // and pass `0` for the modifiers, exactly because HotSpot's
    // `MHN_resolve_Mem` overwrites them with the resolved member's real access
    // flags. CratonVM's resolve never did, so **every** MemberName resolved
    // through this native reported `isStatic() == false`:
    //
    //     public MethodType getInvocationType() {
    //         MethodType itype = getMethodOrFieldType();
    //         ...
    //         if (!isStatic())  return itype.insertParameterTypes(0, clazz);
    //
    // — a phantom receiver parameter prepended to every static method's type.
    // Nothing read it back with assertions off, which is why it survived; with
    // `-ea` the very first `NamedFunction` built this way trips
    // `LambdaForm$Name`'s constructor during `java.lang.invoke` boot:
    //
    //     AssertionError: arity mismatch: arguments.length=1 ==
    //       function.arity()=2 in t851:L=DirectMethodHandle.allocateInstance(a0:L)
    //
    // OR-ed in rather than assigned: the kind bit and reference kind already in
    // `flags` are the caller's request and must survive resolution.
    let mut resolved_access: Option<u16> = None;
    // The reference kind resolution settles on (HotSpot's `init_field_MemberName`
    // rewrites a field's get/put kind by the field's static-ness; see below).
    let mut resolved_ref_kind = ref_kind;
    // Wave 23 (lane L4): the checks of `MHN_RESOLVE_REPORTS_ABSENT_MEMBERS`,
    // on a real `MemberName` only (the fabricated synthetic-JDK layout keeps
    // its permissive resolve).
    let report_absent = MHN_RESOLVE_REPORTS_ABSENT_MEMBERS
        && resolved_class_id.is_some()
        && mirror_cid.is_some()
        && !name.is_empty()
        && !synthetic_layout;

    // If this is a method reference (refKind 5-9), verify the method exists
    if ref_kind >= 5 && ref_kind <= 9 {
        // Read descriptor from the `type` field if it is a MethodType
        if let Value::Object(Some(mt)) = mn_get(ctx, member_name, "type", MN_TYPE) {
            let desc = descriptor_from_method_type(ctx, mt);
            // The exact descriptor, or `None` when it cannot be rebuilt
            // exactly — then nothing below may conclude anything.
            let strict_desc = if report_absent {
                mhn_strict_method_descriptor(&*ctx, mt)
            } else {
                None
            };
            if !name.is_empty() && !desc.is_empty() {
                // invokeVirtual / invokeSpecial / invokeInterface also resolve
                // through superinterfaces (see `lookup_require_method`), which
                // `method_exists` does not walk; invokeStatic and
                // newInvokeSpecial never do.
                let exists = ctx.method_exists(&class_name, &name, &desc)
                    || (matches!(ref_kind, 5 | 7 | 9)
                        && lk_superinterface_declares_instance_method(
                            &*ctx,
                            &class_name,
                            &name,
                            &desc,
                        ));
                let speculative = matches!(args.get(3), Some(Value::Int(1)));
                method_found = exists;
                if !exists {
                    // Method not found — for speculative resolve, return null.
                    // Unless the wave-23 search FINDS it where `method_exists`
                    // does not look (a superinterface's default method, a
                    // class `method_exists` cannot name): `resolveOrNull`
                    // answers the member there on HotSpot.
                    let found_elsewhere = match (mirror_cid, strict_desc.as_deref()) {
                        (Some(cid), Some(strict)) if speculative => matches!(
                            mhn_method_verdict(&*ctx, cid, &name, strict),
                            MhnVerdict::Found(_)
                        ),
                        _ => false,
                    };
                    if speculative && !found_elsewhere {
                        ctx.unpin_native_roots(member_name_pin);
                        return Ok(Some(Value::Object(None)));
                    }
                }
                // Wave 23: what HotSpot's resolution raises, when it can be
                // proven — an absent method (`NoSuchMethodError`), or one whose
                // static-ness the reference kind contradicts
                // (`IncompatibleClassChangeError`: `LinkResolver`'s "Expected
                // static method" / "Expected instance not static method").
                // `Lookup.find*` maps them to `NoSuchMethodException` /
                // `IllegalAccessException`.
                let mut found_access: Option<u16> = None;
                // The class whose declaration resolution found (wave 83).
                let mut found_holder: Option<ClassId> = None;
                if let (Some(cid), Some(strict)) = (mirror_cid, strict_desc.as_deref()) {
                    let (verdict, holder) = if exists && strict != desc {
                        (MhnVerdict::Unknown, None)
                    } else {
                        mhn_method_resolution(&*ctx, cid, &name, strict)
                    };
                    if let MhnVerdict::Found(access) = verdict {
                        found_access = Some(access);
                        method_found = true;
                        found_holder = holder;
                    }
                    let failure = match verdict {
                        // `method_exists` walks superclasses, so for a
                        // constructor (never inherited) its `true` may be a
                        // superclass's `<init>`; the verdict looked at the
                        // class's own only.
                        MhnVerdict::Absent if !exists || name == "<init>" => {
                            Some(VmError::Linkage(LinkageError::NoSuchMethodError {
                                class_name: class_name.clone(),
                                method_name: name.clone(),
                                method_descriptor: strict.to_string(),
                            }))
                        }
                        MhnVerdict::Found(access) => {
                            let is_static = access & MHN_ACC_STATIC != 0;
                            // 6 = REF_invokeStatic; 5, 7, 9 = virtual, special,
                            // interface; 8 = newInvokeSpecial (a constructor).
                            let wrong = match ref_kind {
                                6 => !is_static,
                                5 | 7 | 9 => is_static,
                                _ => false,
                            };
                            // Interpreter round i7 wave 137, lane L4
                            // (`CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS`, parity,
                            // default on): `LinkResolver`'s own texts, naming the
                            // resolved method as `print_external_name` does
                            // (`L4W137FindSpecialAccess` rows `static`,
                            // `virtual-of-static`, `static-of-instance`,
                            // `iface-virtual-of-static`), and the invokeSpecial
                            // direct-superinterface check (row `iface-indirect`).
                            if wrong {
                                let message = if mhn_resolve_special_link_checks_enabled() {
                                    mhn_static_mismatch_message(
                                        &*ctx,
                                        ref_kind,
                                        ctx.is_interface_class(cid),
                                        holder.unwrap_or(cid),
                                        &name,
                                        strict,
                                    )
                                } else {
                                    format!(
                                        "Expected {} method {}.{}{}",
                                        if is_static {
                                            "instance not static"
                                        } else {
                                            "static"
                                        },
                                        class_name.class_text(&*ctx),
                                        name,
                                        strict
                                    )
                                };
                                Some(VmError::Linkage(LinkageError::IncompatibleClassChangeError {
                                    message,
                                }))
                            } else if ref_kind == 7
                                && ctx.is_interface_class(cid)
                                && mhn_resolve_special_link_checks_enabled()
                            {
                                mhn_special_indirect_superinterface(
                                    &*ctx,
                                    cid,
                                    caller_cid_early,
                                    holder.unwrap_or(cid),
                                    &name,
                                    strict,
                                )
                                .map(|message| {
                                    VmError::Linkage(LinkageError::IncompatibleClassChangeError {
                                        message,
                                    })
                                })
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    // SBR3 lane mh: HotSpot's `MHN_resolve_Mem` checks the member's
                    // accessibility from `caller` (null = trusted lookup: skipped)
                    // and raises `IllegalAccessError`, which the JDK's
                    // `MemberName.Factory.resolve` turns into an unresolved member:
                    // `Lookup.find*` then reports `IllegalAccessException: no such
                    // method: ..` (not the Java-side "member is private to package").
                    // Only a member the named class declares itself is judged; one
                    // inherited keeps the Java-side check.
                    //
                    // A private member's nest hosts are loaded first, as
                    // `MHN_resolve_Mem` -> `InstanceKlass::nest_host` does (JIT
                    // round 20 wave 16, lane nest;
                    // `lang_class::settle_reflective_nest_hosts`). A GC point
                    // only when a host is loaded; nothing held here is read
                    // after it but `member_name`, which is re-read through
                    // `member_name_pin` before its next use, and `args`' ints.
                    if let (MhnVerdict::Found(access), Some(caller_cid), false) = (
                        verdict,
                        caller_cid_early,
                        matches!(args.get(2), Some(Value::Int(-1))),
                    ) {
                        crate::lang_class::settle_reflective_nest_hosts(
                            ctx,
                            caller_cid,
                            cid,
                            i32::from(access),
                            &mut [],
                            &mut [],
                            &[],
                        );
                    }
                    // Interpreter round i7 wave 138, lane L4
                    // (`CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS`, parity, default
                    // on; `L4W138ResolveCheckOrder` rows `static-of-private-instance`,
                    // `virtual-of-private-static`, `special-of-private-static`):
                    // `LinkResolver::resolve_method` / `resolve_interface_method`
                    // check the caller's access (step 5) before the
                    // `linktime_resolve_*` static-ness and direct-superinterface
                    // checks, so a member failing both is HotSpot's
                    // `IllegalAccessError`. `=0`: the link check first, as before.
                    let access_first = failure.is_some()
                        && matches!(verdict, MhnVerdict::Found(_))
                        && mhn_resolve_special_link_checks_enabled();
                    let mut access_failure = || {
                        let MhnVerdict::Found(access) = verdict else {
                            return None;
                        };
                        // `LM_TRUSTED` (-1): the JDK's own resolver
                        // (`InvokerBytecodeGenerator.resolveInvokerMember`, caller
                        // `LambdaForm` over a spun hidden class's private method)
                        // asks for no access check.
                        if matches!(args.get(2), Some(Value::Int(-1))) {
                            return None;
                        }
                        let caller_cid = caller_cid_early?;
                        let declares_it = mhn_declared_methods_named(&*ctx, cid, &name)
                            .0
                            .iter()
                            .any(|m| m.name == name && m.descriptor == strict);
                        if !declares_it
                            || crate::lang_reflect::caller_may_access_member(
                                ctx,
                                caller_cid,
                                cid,
                                i32::from(access),
                                None,
                            )
                        {
                            return None;
                        }
                        // HotSpot 25's `check_method_accessability` text
                        // (`class C tried to access private method 'void
                        // D.m()' (C and D are in unnamed module of loader
                        // 'app')`), the cause `Lookup.find*`'s
                        // `IllegalAccessException` carries (JIT round 21 wave
                        // 10, lane indy; `CRATONVM_MHN_RESOLVE_ACCESS_TEXT`,
                        // `=0` the pre-JDK-11 text below).
                        if mhn_resolve_access_text_enabled() {
                            return Some(VmError::Linkage(LinkageError::IllegalAccessError {
                                message: mhn_method_access_denied_message(
                                    ctx,
                                    caller_cid,
                                    cid,
                                    &class_name,
                                    &name,
                                    strict,
                                    access,
                                ),
                            }));
                        }
                        // A hidden class keeps its `/0x<hex>` tail, as
                        // HotSpot's `external_name()` spells it (r21 wave 8).
                        let holder_text = crate::lang_reflect::external_name_of_maybe_hidden(
                            &class_name,
                            ctx.is_class_hidden(cid),
                        );
                        Some(VmError::Linkage(LinkageError::IllegalAccessError {
                            message: format!(
                                "tried to access method {}.{}{}",
                                holder_text,
                                name,
                                strict
                            ),
                        }))
                    };
                    let failure = if access_first {
                        access_failure().or(failure)
                    } else {
                        failure.or_else(access_failure)
                    };
                    if let Some(error) = failure {
                        ctx.unpin_native_roots(member_name_pin);
                        // A speculative resolve answers a resolution error
                        // with null (HotSpot clears it), as it does absence.
                        if speculative {
                            return Ok(Some(Value::Object(None)));
                        }
                        return Err(MethodCallFailed::InternalError(error));
                    }
                }
                // Not gated on `exists`. `method_exists` matches the exact
                // descriptor, which a signature-polymorphic member never has —
                // `MethodHandle.linkToSpecial` is declared `(Object...)Object`
                // and the MemberName carries the call site's `(L,L)V`. Gating
                // the modifier lookup on `exists` therefore skipped exactly the
                // members that need the polymorphic fallback inside
                // `declared_method_access_flags`, and left `linkToSpecial`
                // without its `ACC_STATIC`.
                //
                // Interpreter round i5 wave 105, lane L4 (probe
                // `L4W105ResolveOverloadFlags`): the flags the wave-23 search
                // FOUND come first -- they are the resolved method's own (exact
                // descriptor, JVMS 5.4.3.3 / 5.4.3.4 order), as HotSpot's
                // `init_method_MemberName` writes them. `declared_method_access_flags`
                // is only the fallback for what that search cannot answer
                // (signature-polymorphic members, inexact descriptors): its
                // "one method of the name" rule took a DIFFERENT overload a
                // subclass declares (`Sub.foo(int)` static for `Base.foo()`),
                // so `findVirtual(Sub, "foo", ()V)` failed "expected a
                // non-static method" and a private overload made the inherited
                // public method private. A superinterface's method (a default
                // reached through a class) is the search's answer too.
                resolved_access = found_access;
                if resolved_access.is_none() {
                    if let Some(cid) = resolved_class_id {
                        resolved_access = declared_method_access_flags(ctx, cid, &name, &desc);
                    }
                }
                // Interpreter round i5 wave 108, lane L3 (probe
                // `L3W108FinalVirtualHandleKind`; page
                // `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`): only
                // a method the wave-23 search FOUND (its own flags and holder).
                if matches!(ref_kind, 5 | 9) {
                    if let (Some(access), Some(holder)) = (found_access, found_holder) {
                        direct_call = mhn_virtual_is_direct_call(&*ctx, holder, access);
                    }
                }
                // Wave 108 hand-back: a found constructor is a `direct_call`
                // too (`init_method_MemberName`'s `is_initializer` arm writes
                // `IS_CONSTRUCTOR | REF_invokeSpecial`). `Lookup.checkAccess`'s
                // refusal prints that kind (`L4W48LookupFinderAccessText` rows
                // `pkg-private-ctor`, `mod-package-ctor`), and
                // `DirectMethodHandle.makeAllocator`'s `asConstructor()` turns
                // it back into `REF_newInvokeSpecial` for the handle's member.
                if ref_kind == 8 && name == "<init>" && method_found {
                    direct_call = true;
                }
                // The method's DECLARING class (interpreter round i3 wave 83;
                // page `i83-orch-reveal-direct-names-the-referring-class-of-an-inherited-static`):
                // HotSpot's `init_method_MemberName` writes the resolved
                // method's holder into `clazz` -- `findStatic(Q2, "pst")` of
                // `P2.pst` reveals `invokeStatic P2.pst`, `findVirtual(Sub,
                // "m")` of `Base.m` `invokeVirtual Base.m`, an interface's
                // `findVirtual` of an `Object` method `invokeVirtual
                // Object.hashCode` (the itable call becomes a vtable one).
                // Kept, `revealDirect` and `getDeclaringClass()` named the
                // referring class. The holder is the one the wave-23 search
                // found (JVMS 5.4.3.3 / 5.4.3.4 order; exact descriptors only),
                // so this costs no second walk. A default method a CLASS
                // inherits keeps the class (HotSpot's vtable call answers the
                // resolved class).
                if matches!(ref_kind, 5 | 6 | 7 | 9) {
                    if let (Some(named), Some(holder)) = (mirror_cid, found_holder) {
                        if let Some(holder) = resolved_method_holder(&*ctx, named, holder) {
                            let to_object = ref_kind == 9
                                && ctx.class_id_by_name("java/lang/Object") == Some(holder);
                            method_holder = Some((holder, if to_object { 5 } else { ref_kind }));
                        }
                    }
                }
                // Wave 95 (lane L4, `CRATONVM_MHN_CALLER_SENSITIVE_FLAG`): HotSpot's
                // `init_method_MemberName` marks a caller-sensitive method.
                if method_found && matches!(ref_kind, 5 | 6 | 7 | 9) {
                    if let Some(holder) = found_holder.or(mirror_cid) {
                        caller_sensitive = mhn_method_is_caller_sensitive(
                            &*ctx,
                            holder,
                            &name,
                            Some(strict_desc.as_deref().unwrap_or(desc.as_str())),
                        );
                    }
                }
            }
        }
    } else if (1..=4).contains(&ref_kind) {
        // Field kinds. A field name is unique within its declaring class, so
        // no descriptor match is needed — and `type` here is a Class mirror,
        // not a MethodType, so there is no descriptor to read anyway.
        if let Some(cid) = resolved_class_id {
            if !name.is_empty() {
                resolved_access = declared_field_access_flags(ctx, cid, &name);
            }
        }
        // Wave 23 (lane L4): HotSpot resolves a field by name AND type, and
        // raises `NoSuchFieldError` (-> `NoSuchFieldException`) when none
        // matches; a field of the other static-ness is found, and the
        // reference kind is rewritten to the field's own (`getField` <->
        // `getStatic`, `putField` <-> `putStatic`), which `Lookup.checkField`
        // then refuses as "expected a (non-)static field: ...". Only when the
        // type descriptor is exact (not a hidden class).
        if report_absent {
            let field_desc = match mn_get(ctx, member_name, "type", MN_TYPE) {
                // A `Class` (a primitive's mirror included); `type` may also
                // hold a descriptor `String`, which proves nothing here.
                Value::Object(Some(ty))
                    if ctx.class_name_of_id(ctx.class_id_of_object(ty)).as_deref()
                        == Some("java/lang/Class") =>
                {
                    mhn_strict_type_descriptor(&*ctx, ty)
                }
                _ => None,
            };
            if let (Some(cid), Some(field_desc)) = (mirror_cid, field_desc) {
                match mhn_field_verdict(&*ctx, cid, &name, &field_desc) {
                    MhnVerdict::Absent => {
                        ctx.unpin_native_roots(member_name_pin);
                        // Speculative: absence is a null answer, not an error.
                        if matches!(args.get(3), Some(Value::Int(1))) {
                            return Ok(Some(Value::Object(None)));
                        }
                        return Err(MethodCallFailed::InternalError(VmError::Linkage(
                            LinkageError::NoSuchFieldError {
                                class_name: class_name.clone(),
                                field_name: name.clone(),
                                field_descriptor: field_desc,
                            },
                        )));
                    }
                    MhnVerdict::Found(access) => {
                        // The flags the type-matched field has, not the
                        // same-named one `declared_field_access_flags` found.
                        resolved_access = Some(access);
                        field_found = true;
                        let is_static = access & MHN_ACC_STATIC != 0;
                        resolved_ref_kind = match (ref_kind, is_static) {
                            (1, true) | (3, true) => ref_kind + 1,
                            (2, false) | (4, false) => ref_kind - 1,
                            _ => ref_kind,
                        };
                    }
                    MhnVerdict::Unknown => {}
                }
            }
        }
    }

    // A STATIC field the named class inherits (from a superinterface or a
    // superclass) resolves to its DECLARING class, as HotSpot's
    // `init_field_MemberName` writes `fd.field_holder()` into `clazz`. The JDK's
    // `StaticAccessor` reads, writes and initializes `getDeclaringClass()`, and
    // `staticFieldBase` / `staticFieldOffset` look the field up there: with the
    // named class kept, every inherited static field handle under `--jdk-only`
    // raised `UnsupportedOperationException: real static field MethodHandle:
    // unregistered offset`, a `VarHandle` over one `NullPointerException`, and
    // the setter's refusal named `B.JX` for `J.JX` (interpreter round i2 wave 62,
    // lane L4; `L4W62InheritedStaticFieldHandles`).
    //
    // An INSTANCE field the named class inherits resolves to its declaring
    // class too (interpreter round i3 wave 79, lane L4; page
    // `i74-L4-a-finder-on-a-subclass-reads-an-inherited-instance-field-at-slot-zero`):
    // `init_field_MemberName` writes the holder for every field. Kept, the
    // named class sent `objectFieldOffset` looking among ITS own fields, which
    // missed and answered the vmindex fallback `0` -- `findGetter(Derived, "b")`
    // read `Base.a` (slot 0), a setter wrote it, and `revealDirect` named
    // `Derived`. Real layouts only (the synthetic layout's readers resolve the
    // slot by name through the named class).
    let inherited_holder = if name.is_empty() {
        None
    } else if matches!(resolved_ref_kind, 2 | 4) {
        mirror_cid.and_then(|cid| inherited_static_field_holder(&*ctx, cid, &name))
    } else if matches!(resolved_ref_kind, 1 | 3) && !synthetic_layout {
        mirror_cid.and_then(|cid| inherited_instance_field_holder(&*ctx, cid, &name))
    } else {
        None
    };
    let inherited_holder = match (inherited_holder, method_holder) {
        (None, Some((holder, kind))) => {
            resolved_ref_kind = kind;
            Some(holder)
        }
        (field_holder, _) => field_holder,
    };
    // Interpreter round i5 wave 108, lane L3: a `vtable_call` HotSpot binds
    // statically is a `direct_call`, `REF_invokeSpecial` ("a direct call, not
    // necessarily an invokespecial"): `DirectMethodHandle.preparedLambdaForm`
    // then picks `DirectMethodHandle$Holder.invokeSpecial`, and
    // `MethodHandleInfo` maps it back to `invokeVirtual` (`isInvokeSpecial()`
    // is false on the plain handle). An interface call that resolved to an
    // `Object` method arrives here as `invokeVirtual` (wave 83's
    // `to_object`); a private interface method stays an `invokeInterface`
    // request whose member is `invokeSpecial` (`LF_INVSPECIAL_IFC`).
    // `native_mhn_init` has written this for a reflected `Method` since SBR3.
    // `CRATONVM_MHN_RESOLVE_DIRECT_CALL=0` keeps the requested kind.
    if direct_call
        && matches!(resolved_ref_kind, 5 | 8 | 9)
        && crate::nbflags().mhn_resolve_direct_call
    {
        resolved_ref_kind = 7;
    }
    if let Some(holder) = inherited_holder {
        // A mirror allocation, a GC point: `member_name` is re-read below.
        let holder_mirror = ctx.get_class_mirror(holder);
        let member_name = ctx.read_native_pin(member_name_pin, member_name);
        mn_set(
            ctx,
            member_name,
            "clazz",
            MN_CLAZZ,
            Value::Object(Some(holder_mirror)),
        );
        holder_written = Some(holder);
    }
    // `declared_methods`/`declared_fields` allocate, so re-read through the pin
    // before the write.
    let member_name = ctx.read_native_pin(member_name_pin, member_name);
    // The reference kind resolution settled on (differs from the request only
    // for a field of the other static-ness, wave 23).
    let flags_with_kind = (flags & !(0x0F << 24)) | (resolved_ref_kind << 24);
    // Wave 95: `MN_CALLER_SENSITIVE`, written with the access flags.
    let flags_with_kind = if caller_sensitive {
        flags_with_kind | MN_CALLER_SENSITIVE
    } else {
        flags_with_kind
    };
    if let Some(access) = resolved_access {
        let merged = flags_with_kind | (i32::from(access) & 0xFFFF);
        if merged != flags {
            mn_set(ctx, member_name, "flags", MN_FLAGS, Value::Int(merged));
            flags_written = Some(merged);
        }
    } else if flags_with_kind != flags {
        mn_set(
            ctx,
            member_name,
            "flags",
            MN_FLAGS,
            Value::Int(flags_with_kind),
        );
        flags_written = Some(flags_with_kind);
    }

    ctx.unpin_native_roots(member_name_pin);
    // Wave 73 (`CRATONVM_MHN_RESOLVE_MEMO`): remember a FOUND field member
    // (`field_found` implies a real layout, a field kind and the type-matched
    // verdict); `mhn_memo_put` drops it when a collection ran during the call.
    if let Some(k) = memo_key {
        if field_found {
            mhn_memo_put(
                &*ctx,
                k.epoch,
                MhnMemoKind::Resolve,
                k.key,
                k.flags,
                MhnMemoAnswer::Resolve {
                    flags: flags_written,
                    holder: holder_written,
                },
            );
        }
    }
    // Wave 81 (`CRATONVM_MHN_RESOLVE_METHOD_MEMO`): remember a FOUND method
    // member whose class came from its mirror (a real layout and a method kind
    // are implied by the key); dropped when a collection ran during the call.
    if let Some(k) = method_memo_key {
        if method_found && mirror_cid.is_some() && holder_written.is_none() {
            mhn_method_memo_put(&*ctx, k.epoch, k.key, flags_written);
        }
    }
    Ok(Some(Value::Object(Some(member_name))))
}

/// Declared access flags of `name`+`descriptor`, searched from `class_id` up
/// the superclass chain.
///
/// Superclasses only, no interface step: a `REF_invokeInterface` member names
/// the interface itself as its declaring class, so the first hop already covers
/// it, and a default method inherited from an interface is not something this
/// native is asked to resolve against a class receiver.
///
/// Bounded, so a self-referential hierarchy in a fabricated/synthetic class
/// cannot spin here.
pub(crate) fn declared_method_access_flags(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<u16> {
    let mut cur = Some(class_id);
    for _ in 0..64 {
        let cid = cur?;
        let by_name: Vec<_> = mhn_declared_methods_named(&*ctx, cid, name)
            .0
            .into_iter()
            .filter(|m| m.name == name)
            .collect();
        if let Some(m) = by_name.iter().find(|m| m.descriptor == descriptor) {
            return Some(m.access_flags);
        }
        // Signature-polymorphic methods (`MethodHandle.invoke`, `invokeExact`,
        // `invokeBasic`, `linkToStatic`, `linkToSpecial`, …) are DECLARED as
        // `(Object...)Object`, but a `MemberName` for one carries the *call
        // site's* type — `(L,L)V` and the like. There is no descriptor to match
        // on, so the exact search above finds nothing, which left `linkToSpecial`
        // without its `ACC_STATIC` and produced the second `arity mismatch`
        // assertion after `allocateInstance`.
        //
        // A name declared exactly once in the class is unambiguous, so take it.
        // Guarded on uniqueness deliberately: with real overloads present,
        // "some overload's flags" would be a guess, and no flags at all is
        // better than confidently wrong ones.
        if by_name.len() == 1 {
            return Some(by_name[0].access_flags);
        }
        cur = ctx.superclass_of(cid);
    }
    None
}

/// Declared access flags of the field `name`, searched from `class_id` up the
/// superclass chain. See [`declared_method_access_flags`].
pub(crate) fn declared_field_access_flags(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<u16> {
    let mut cur = Some(class_id);
    for _ in 0..64 {
        let cid = cur?;
        if let Some(f) = mhn_declared_fields_named(&*ctx, cid, name)
            .0
            .into_iter()
            .find(|f| f.name == name)
        {
            return Some(f.access_flags);
        }
        cur = ctx.superclass_of(cid);
    }
    None
}

/// The fields `class_id` declares named `name`, and whether it declares any
/// field (interpreter round i5 wave 102, lane L4; page
/// `i89-L4-method-finder-per-class-and-per-handle-cost`, "Wave 102"): the field
/// half of [`mhn_declared_methods_named`]. `CRATONVM_MHN_RESOLVE_NAMED_FIELDS`
/// (default off) asks the VM for the matches only
/// (`NativeClassAccess::declared_fields_named`); off, the whole table is copied
/// and filtered here, as before. Every caller filters by `name` itself.
pub(crate) fn mhn_declared_fields_named(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> (Vec<cratonvm_native_api::FieldMetadata>, bool) {
    if crate::nbflags().mhn_resolve_named_fields {
        return ctx.declared_fields_named(class_id, name);
    }
    let all = ctx.declared_fields(class_id);
    let any = !all.is_empty();
    (all.into_iter().filter(|f| f.name == name).collect(), any)
}

/// Kill switch (interpreter round i1 wave 23, lane L4): `MethodHandleNatives.resolve`
/// raises the resolution error HotSpot's `MHN_resolve_Mem` raises for a member
/// it can PROVE is absent or of the wrong static-ness, instead of returning
/// the `MemberName` as resolved.
///
/// Why it matters: `MemberName.Factory.resolveOrFail` turns that error into
/// the checked exception of `Lookup.find*` (`NoSuchMethodError` ->
/// `NoSuchMethodException`, `NoSuchFieldError` -> `NoSuchFieldException`,
/// `IncompatibleClassChangeError` -> `IllegalAccessException`), which is how
/// every version-probing library feature-tests an API
/// (`try { findVirtual(..) } catch (ReflectiveOperationException e) { fallback }`).
/// Returning an absent member as resolved handed back a handle whose first
/// call fails later with an `Error`, past that `catch`. Probe:
/// `tools/probes/interp/L4/L4W23LookupResolveErrors.java`.
///
/// "Prove" is deliberate: [`mhn_method_verdict`] and [`mhn_field_verdict`]
/// answer `Unknown` (and resolution proceeds as before) whenever the evidence
/// is incomplete — a synthetic `MemberName` layout, a descriptor that cannot be
/// rebuilt exactly, a hidden class in it, a signature-polymorphic name, a
/// compatibility stub or registered native anywhere on the path, or a
/// hierarchy that enumerates no members at all. `false` restores the
/// pre-wave-23 permissive resolve.
pub(crate) const MHN_RESOLVE_REPORTS_ABSENT_MEMBERS: bool = true;

/// What [`mhn_method_verdict`] / [`mhn_field_verdict`] could establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MhnVerdict {
    /// Resolution finds this member; its declared access flags.
    Found(u16),
    /// Resolution provably finds nothing.
    Absent,
    /// Not enough evidence either way: resolve as before.
    Unknown,
}

pub(crate) const MHN_ACC_PRIVATE: u16 = 0x0002;
/// Does HotSpot bind a virtual (or interface-to-`Object`) call of the method
/// `holder` declares with `access` statically -- a `CallInfo::direct_call`,
/// which `init_method_MemberName` reports as `REF_invokeSpecial`?
/// `Method::can_be_statically_bound`: a private method (no vtable entry), a
/// final one, or any method of a final class. Of a method an interface
/// declares, only a private one (`runtime_resolve_interface_method`: no itable
/// index); a default or overpass method reuses a vtable entry. Reads only.
/// Interpreter round i5 wave 108, lane L3.
pub(crate) fn mhn_virtual_is_direct_call(ctx: &dyn NativeContext, holder: ClassId, access: u16) -> bool {
    const ACC_FINAL: u16 = 0x0010;
    if access & MHN_ACC_STATIC != 0 {
        return false;
    }
    if access & MHN_ACC_PRIVATE != 0 {
        return true;
    }
    !ctx.is_interface_class(holder)
        && (access & ACC_FINAL != 0 || ctx.class_access_flags(holder) & ACC_FINAL != 0)
}

pub(crate) const MHN_ACC_STATIC: u16 = 0x0008;
pub(crate) const MHN_ACC_VARARGS: u16 = 0x0080;
pub(crate) const MHN_ACC_NATIVE: u16 = 0x0100;
pub(crate) const MHN_ACC_ABSTRACT: u16 = 0x0400;

/// `CRATONVM_MHN_RESOLVE_ACCESS_TEXT` -- correctness, default ON (JIT round 21
/// wave 10, lane indy; item 2 of
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// `MHN_resolve`'s refused method access raises HotSpot 25's
/// `LinkResolver::check_method_accessability` text
/// ([`mhn_method_access_denied_message`]). `=0` restores the pre-JDK-11
/// `tried to access method X.m(desc)`. Read only on a refusal (cold).
pub(crate) fn mhn_resolve_access_text_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MHN_RESOLVE_ACCESS_TEXT")
}

/// `CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS` -- parity, default ON, `=0` off
/// (interpreter round i7 wave 137, lane L4; probe `L4W137FindSpecialAccess`):
/// `MHN_resolve` of a method raises `LinkResolver`'s own
/// `IncompatibleClassChangeError` texts ([`mhn_static_mismatch_message`]) and
/// the invokeSpecial direct-superinterface refusal
/// ([`mhn_special_indirect_superinterface`]). `=0` restores the old
/// `Expected instance not static method C.m()V` text and no refusal. Read only
/// where either can apply (a static-ness mismatch, an invokeSpecial member of
/// an interface).
pub(crate) fn mhn_resolve_special_link_checks_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS")
}

/// The external name of `holder` (a hidden class keeps its `/0x<hex>` tail).
pub(crate) fn mhn_class_external_name(ctx: &dyn NativeContext, holder: ClassId) -> String {
    match ctx.class_name_of_id(holder) {
        Some(n) => crate::lang_reflect::external_name_of_maybe_hidden(&n, ctx.is_class_hidden(holder)),
        None => String::new(),
    }
}

/// HotSpot 25's `LinkResolver` text for a resolved method of the other
/// static-ness, the method named by `Method::print_external_name` (its
/// HOLDER, the class that declares it): `linktime_resolve_static_method`'s
/// `Expected static method '<m>'` for `REF_invokeStatic`;
/// `resolve_interface_method`'s `Expected instance not static method '<m>'`
/// for any other kind through an interface reference (`refc_is_interface`);
/// `linktime_resolve_virtual_method` / `_special_method`'s `Expecting
/// non-static method '<m>'` through a class reference. The same split the
/// bytecode link words (`interpreter::invoke::interface_method_static_wording`).
pub(crate) fn mhn_static_mismatch_message(
    ctx: &dyn NativeContext,
    ref_kind: i32,
    refc_is_interface: bool,
    holder: ClassId,
    name: &str,
    desc: &str,
) -> String {
    let holder_ext = mhn_class_external_name(ctx, holder);
    let method = mhn_external_method_name(&holder_ext, name, desc)
        .unwrap_or_else(|| format!("{holder_ext}.{name}{desc}"));
    if ref_kind == 6 {
        format!("Expected static method '{method}'")
    } else if refc_is_interface {
        format!("Expected instance not static method '{method}'")
    } else {
        format!("Expecting non-static method '{method}'")
    }
}

/// `LinkResolver::linktime_resolve_special_method`'s check (JDK 25): an
/// invokeSpecial member resolved through the interface `refc` must be named
/// through the caller itself or one of its DIRECT superinterfaces
/// (`InstanceKlass::is_same_or_direct_interface`: the caller's own
/// `interfaces`, not its superclass's, not a superinterface's); otherwise
/// `IncompatibleClassChangeError: Interface method reference: '<m>', is not in
/// a direct superinterface of <caller>`, which `Lookup.findSpecial` /
/// `unreflectSpecial` report as `IllegalAccessException: no such method`.
/// `None` when the check passes, or with no caller (a trusted lookup).
pub(crate) fn mhn_special_indirect_superinterface(
    ctx: &dyn NativeContext,
    refc: ClassId,
    caller: Option<ClassId>,
    holder: ClassId,
    name: &str,
    desc: &str,
) -> Option<String> {
    let caller = caller?;
    if caller == refc || ctx.class_interfaces(caller).contains(&refc) {
        return None;
    }
    // Interpreter round i7 wave 138, lane L4 (`L4W138ResolveCheckOrder` rows
    // `lambda-special-*`): a lambda class the VM minted outside the class
    // store (a `Lookup` in it, `privateLookupIn`) has no `interfaces` there;
    // its direct superinterfaces are its record's (the functional interface,
    // the markers, `Serializable`), as `InnerClassLambdaMetafactory` spins them.
    let proxy_name = crate::lang_class::lambda_proxy_class_name(ctx, caller);
    if proxy_name.is_some()
        && ctx
            .lambda_proxy_record(caller)
            .is_some_and(|record| record.interfaces.contains(&refc))
    {
        return None;
    }
    let holder_ext = mhn_class_external_name(ctx, holder);
    let method = mhn_external_method_name(&holder_ext, name, desc)
        .unwrap_or_else(|| format!("{holder_ext}.{name}{desc}"));
    Some(format!(
        "Interface method reference: '{method}', is not in a direct superinterface of {}",
        proxy_name.unwrap_or_else(|| mhn_class_external_name(ctx, caller))
    ))
}

/// One field type of a descriptor, as `Symbol::print_as_signature_external_*`
/// spells it (`int`, `java.lang.String`, `long[][]`), and the rest of `d`.
pub(crate) fn mhn_external_field_type(d: &str) -> Option<(String, &str)> {
    let dims = d.bytes().take_while(|&b| b == b'[').count();
    let rest = &d[dims..];
    let (base, tail) = match *rest.as_bytes().first()? {
        b'B' => ("byte".to_string(), &rest[1..]),
        b'C' => ("char".to_string(), &rest[1..]),
        b'D' => ("double".to_string(), &rest[1..]),
        b'F' => ("float".to_string(), &rest[1..]),
        b'I' => ("int".to_string(), &rest[1..]),
        b'J' => ("long".to_string(), &rest[1..]),
        b'S' => ("short".to_string(), &rest[1..]),
        b'Z' => ("boolean".to_string(), &rest[1..]),
        b'V' if dims == 0 => ("void".to_string(), &rest[1..]),
        b'L' => {
            let end = rest.find(';')?;
            (rest[1..end].replace('/', "."), &rest[end + 1..])
        }
        _ => return None,
    };
    Some((format!("{base}{}", "[]".repeat(dims)), tail))
}

/// `Method::print_external_name` (JDK 25): `<return> <holder>.<name>(<p1>,
/// <p2>)`, e.g. `void p.D.m(int, java.lang.String)`; `holder_ext` is the
/// holder's `external_name()` already. `None` for a malformed descriptor.
pub(crate) fn mhn_external_method_name(holder_ext: &str, name: &str, desc: &str) -> Option<String> {
    let inner = desc.strip_prefix('(')?;
    let close = inner.find(')')?;
    let mut params = &inner[..close];
    let mut out: Vec<String> = Vec::new();
    while !params.is_empty() {
        let (t, rest) = mhn_external_field_type(params)?;
        out.push(t);
        params = rest;
    }
    let (ret, tail) = mhn_external_field_type(&inner[close + 1..])?;
    if !tail.is_empty() {
        return None;
    }
    Some(format!("{ret} {holder_ext}.{name}({})", out.join(", ")))
}

/// `LinkResolver::check_method_accessability`'s format (JDK 25):
/// `class %s tried to access %s%s%smethod '%s' (%s%s%s)` -- abstract,
/// protected, private in that order -- with the module clause left out when it
/// cannot be built (a loader that cannot be named), as the VM's bytecode texts
/// (`access_control::method_access_denied_message_naming`) leave it out.
pub(crate) fn mhn_method_access_denied_text(
    accessor_ext: &str,
    access: u16,
    external_method: &str,
    clause: Option<&str>,
) -> String {
    const ACC_PROTECTED: u16 = 0x0004;
    let member = format!(
        "{}{}{}method '{external_method}'",
        if access & MHN_ACC_ABSTRACT != 0 {
            "abstract "
        } else {
            ""
        },
        if access & ACC_PROTECTED != 0 {
            "protected "
        } else {
            ""
        },
        if access & MHN_ACC_PRIVATE != 0 {
            "private "
        } else {
            ""
        },
    );
    match clause {
        Some(clause) => format!("class {accessor_ext} tried to access {member} ({clause})"),
        None => format!("class {accessor_ext} tried to access {member}"),
    }
}

/// HotSpot's module clause of a member refusal: one module,
/// `Klass::joint_in_module_of_loader` (`C and D are in unnamed module of
/// loader 'app'`, the holder's module and loader); two,
/// `class_in_module_of_loader` of each, accessor first, joined by `; `. Each
/// party is (external name, module text, loader text).
pub(crate) fn mhn_member_module_clause(
    accessor: (&str, &str, &str),
    holder: (&str, &str, &str),
    same_module: bool,
) -> String {
    let (a, a_module, a_loader) = accessor;
    let (h, h_module, h_loader) = holder;
    if same_module {
        format!("{a} and {h} are in {h_module} of loader {h_loader}")
    } else {
        format!(
            "{a} is in {a_module} of loader {a_loader}; {h} is in {h_module} of loader \
             {h_loader}"
        )
    }
}

/// `loader`'s HotSpot `loader_name_and_id()`: its `ClassLoader.nameAndId`, or
/// `'app'` / `'platform'` for the built-in loaders, whose field this VM leaves
/// null (as `classloader_real::loader_name_and_id_text` reads it).
pub(crate) fn mhn_loader_object_text(ctx: &mut dyn NativeContext, loader: ObjectRef) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(loader, "nameAndId") {
        if let Some(text) = ctx.read_string(s).filter(|t| !t.is_empty()) {
            return Some(text);
        }
    }
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(loader))?;
    match &*class_name {
        "jdk/internal/loader/ClassLoaders$AppClassLoader" => Some("'app'".to_string()),
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader" => Some("'platform'".to_string()),
        _ => None,
    }
}

/// `cid`'s mirror's `classLoader`, `None` for a null one. May create the
/// mirror (a collection point); the caller holds no unpinned reference.
pub(crate) fn mhn_mirror_loader(ctx: &mut dyn NativeContext, cid: ClassId) -> Option<ObjectRef> {
    let mirror = ctx.try_get_class_mirror(cid)?;
    match ctx.get_field_by_name(mirror, "classLoader") {
        Value::Object(Some(loader)) => Some(loader),
        _ => None,
    }
}

/// The loader text HotSpot prints for `cid` in a member refusal: the built-in
/// loaders by name, a bootstrap-defined class of a named module other than
/// `java.base` by its mirror's loader (HotSpot's platform loader defines
/// those, `access_control::message_loader_of`), a user-defined loader by its
/// `nameAndId` when messages name user loaders. `None`: the clause is left
/// out, as the bytecode texts leave it.
pub(crate) fn mhn_message_loader_text(ctx: &mut dyn NativeContext, cid: ClassId) -> Option<String> {
    match ctx.loader_id_of_class_definite(cid)? {
        2 => Some("'app'".to_string()),
        1 => Some("'platform'".to_string()),
        0 => {
            let other_module = ctx
                .module_name_of_class(cid)
                .is_some_and(|m| !m.is_empty() && m != "java.base");
            if other_module {
                if let Some(loader) = mhn_mirror_loader(ctx, cid) {
                    return mhn_loader_object_text(ctx, loader);
                }
            }
            Some("'bootstrap'".to_string())
        }
        n if n >= 3 => {
            if !ctx.messages_name_user_loaders() {
                return None;
            }
            let registered = crate::classloader::loader_object_for_namespace_id_in(
                crate::classloader::loader_tables(&*ctx),
                n as u32,
            );
            let loader = match registered {
                Some(loader) => loader,
                None => mhn_mirror_loader(ctx, cid)?,
            };
            mhn_loader_object_text(ctx, loader)
        }
        _ => None,
    }
}

/// HotSpot 25's `IllegalAccessError` text when `MHN_resolve` refuses `caller`
/// the method `name desc` that `holder` (internal name `holder_name`)
/// declares with `access` (`LinkResolver::check_method_accessability`):
///
/// `class q.C tried to access private method 'void p.D.m()' (q.C and p.D are
/// in unnamed module of loader 'app')`
///
/// The same text the bytecode link raises
/// (`interpreter::field_access::member_access_refusal`); a constructor is
/// `'void p.D.<init>()'`. Hidden classes keep their `/0x<hex>` tail. Not
/// reproduced: the nest-host failure suffix of a refused private access
/// (`print_nest_host_error_on`), present only when a `NestHost` claim failed.
/// JIT round 21 wave 10, lane indy.
pub(crate) fn mhn_method_access_denied_message(
    ctx: &mut dyn NativeContext,
    caller: ClassId,
    holder: ClassId,
    holder_name: &str,
    name: &str,
    desc: &str,
    access: u16,
) -> String {
    let holder_hidden = ctx.is_class_hidden(holder);
    let holder_ext = crate::lang_reflect::external_name_of_maybe_hidden(holder_name, holder_hidden);
    let caller_hidden = ctx.is_class_hidden(caller);
    let caller_ext = match ctx.class_name_of_id(caller) {
        Some(n) => crate::lang_reflect::external_name_of_maybe_hidden(&n, caller_hidden),
        None => String::new(),
    };
    let external_method = mhn_external_method_name(&holder_ext, name, desc)
        .unwrap_or_else(|| format!("{holder_ext}.{name}{desc}"));
    let clause = match (
        mhn_message_loader_text(ctx, caller),
        mhn_message_loader_text(ctx, holder),
    ) {
        (Some(caller_loader), Some(holder_loader)) => {
            let caller_identity = ctx.module_identity_of_class(caller);
            let same_module = caller_identity == ctx.module_identity_of_class(holder)
                && (caller_identity.name().is_some()
                    || ctx.loader_id_of_class_definite(caller)
                        == ctx.loader_id_of_class_definite(holder));
            let caller_module = ctx.module_clause_of_class(caller);
            let holder_module = ctx.module_clause_of_class(holder);
            Some(mhn_member_module_clause(
                (&caller_ext, &caller_module, &caller_loader),
                (&holder_ext, &holder_module, &holder_loader),
                same_module,
            ))
        }
        _ => None,
    };
    mhn_method_access_denied_text(&caller_ext, access, &external_method, clause.as_deref())
}

/// The exact JVM descriptor of the `MethodType` `mt`, or `None` when it cannot
/// be rebuilt exactly (unlike [`descriptor_from_method_type`], which drops an
/// unrecoverable parameter and defaults an unknown name to `Object`): the
/// object is not a `java.lang.invoke.MethodType`, a slot does not hold a class
/// mirror, a mirror has no name, or a name is a hidden class's (`/0x`), whose
/// class-file spelling differs from its mirror's.
pub(crate) fn mhn_strict_method_descriptor(ctx: &dyn NativeContext, mt: ObjectRef) -> Option<String> {
    let mt_class = ctx.class_id_of_object(mt);
    if ctx.class_name_of_id(mt_class).as_deref() != Some("java/lang/invoke/MethodType") {
        return None;
    }
    let mut desc = String::from("(");
    match ctx.get_field(mt, 1) {
        Value::Object(Some(params)) => {
            let len = ctx.array_length(params);
            for i in 0..len {
                let mirror = recover_class_mirror_from_slot(ctx, ctx.get_array_element(params, i))?;
                desc.push_str(&mhn_strict_type_descriptor(ctx, mirror)?);
            }
        }
        _ => return None,
    }
    desc.push(')');
    let ret = recover_class_mirror_from_slot(ctx, ctx.get_field(mt, 0))?;
    desc.push_str(&mhn_strict_type_descriptor(ctx, ret)?);
    Some(desc)
}

/// The descriptor of one class mirror, or `None` when its name is unknown or
/// hidden. See [`mhn_strict_method_descriptor`].
pub(crate) fn mhn_strict_type_descriptor(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<String> {
    let name = resolve_class_name_robust(ctx, mirror)?;
    if name.is_empty() || name.contains("/0x") {
        return None;
    }
    Some(class_name_to_descriptor(&name).into_owned())
}

/// Whether `m`, declared in `declaring`, is signature polymorphic (JVMS
/// 2.9.3): declared in `MethodHandle` or `VarHandle`, one `Object[]`
/// parameter, `ACC_VARARGS` and `ACC_NATIVE`. Such a member resolves for any
/// call-site descriptor.
/// The methods `class_id` declares named `name`, and whether it declares any
/// method (interpreter round i5 wave 100, lane L4; page
/// `i89-L4-method-finder-per-class-and-per-handle-cost`, "Wave 100").
/// `CRATONVM_MHN_RESOLVE_NAMED_METHODS` (default off) asks the VM for the
/// matches only (`NativeClassAccess::declared_methods_named`); off, the whole
/// table is copied and filtered here, as before. Every caller filters by
/// `name` itself, so both answers are the same list.
pub(crate) fn mhn_declared_methods_named(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> (Vec<cratonvm_native_api::MethodMetadata>, bool) {
    if crate::nbflags().mhn_resolve_named_methods {
        return ctx.declared_methods_named(class_id, name);
    }
    let all = ctx.declared_methods(class_id);
    let any = !all.is_empty();
    (all.into_iter().filter(|m| m.name == name).collect(), any)
}

pub(crate) fn mhn_is_signature_polymorphic(
    ctx: &dyn NativeContext,
    declaring: ClassId,
    m: &cratonvm_native_api::MethodMetadata,
) -> bool {
    m.access_flags & (MHN_ACC_VARARGS | MHN_ACC_NATIVE) == (MHN_ACC_VARARGS | MHN_ACC_NATIVE)
        && m.descriptor.starts_with("([Ljava/lang/Object;)")
        && matches!(
            ctx.class_name_of_id(declaring).as_deref(),
            Some("java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle")
        )
}

/// JVMS 5.4.3.3 / 5.4.3.4 method resolution of `name` + `desc` from
/// `class_id`, as far as the declared members can show it: the class and its
/// superclasses first, then every superinterface (transitively), where only a
/// non-static, non-private method counts (a class does not inherit an
/// interface's static or private methods). The caller has already asked
/// `method_exists` of the symbolic owner (which answers `true` for a stub or a
/// registered native on its superclass chain); each interface is checked for
/// a stub here.
///
/// Only `Absent` changes behaviour, and the static-ness of `Found`; see
/// [`MHN_RESOLVE_REPORTS_ABSENT_MEMBERS`].
pub(crate) fn mhn_method_verdict(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> MhnVerdict {
    mhn_method_resolution(ctx, class_id, name, desc).0
}

/// [`mhn_method_verdict`] with the class whose declaration it found -- the
/// resolved method's holder, which `MethodHandleNatives.resolve` writes into
/// the `MemberName` (interpreter round i3 wave 83). `None` unless `Found`.
pub(crate) fn mhn_method_resolution(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> (MhnVerdict, Option<ClassId>) {
    let mut enumerated_any = false;
    let mut chain: Vec<ClassId> = Vec::new();
    let mut cur = Some(class_id);
    for _ in 0..64 {
        let Some(cid) = cur else { break };
        chain.push(cid);
        let (methods, declares_any) = mhn_declared_methods_named(ctx, cid, name);
        enumerated_any |= declares_any;
        for m in methods.iter().filter(|m| m.name == name) {
            if m.descriptor == desc {
                return (MhnVerdict::Found(m.access_flags), Some(cid));
            }
            if mhn_is_signature_polymorphic(ctx, cid, m) {
                return (MhnVerdict::Unknown, None);
            }
        }
        if name == "<init>" {
            // A constructor is not inherited: the class's own, or none.
            return if enumerated_any {
                (MhnVerdict::Absent, None)
            } else {
                (MhnVerdict::Unknown, None)
            };
        }
        let next = ctx.superclass_of(cid);
        if next == Some(cid) {
            break;
        }
        cur = next;
    }
    if cur.is_some() && chain.len() >= 64 {
        return (MhnVerdict::Unknown, None);
    }
    // JVMS 5.4.3.4 step 2: an interface method reference also resolves to a
    // public method of `Object` (`Runnable.toString`), whether or not this VM
    // records `Object` as an interface's superclass.
    if ctx.is_interface_class(class_id) {
        match ctx.class_id_by_name("java/lang/Object") {
            Some(object) if !chain.contains(&object) => {
                let (methods, _) = mhn_declared_methods_named(ctx, object, name);
                if let Some(m) = methods.iter().find(|m| {
                    m.name == name && m.descriptor == desc && m.access_flags & 0x0001 != 0
                }) {
                    return (MhnVerdict::Found(m.access_flags), Some(object));
                }
            }
            Some(_) => {}
            None => return (MhnVerdict::Unknown, None),
        }
    }
    let mut pending: Vec<ClassId> = chain
        .iter()
        .flat_map(|&c| ctx.class_interfaces(c))
        .collect();
    let mut seen: Vec<ClassId> = Vec::new();
    // Interpreter round i5 wave 104, lane L4 (probe
    // `L4W104DefaultThroughSubclassModifiers`): every superinterface
    // declaration is collected, then chosen as HotSpot's
    // `LinkResolver::lookup_method_in_klasses` / `lookup_method_in_interfaces`
    // choose it: the class's (or interface's) `default_methods` first -- the
    // ONE maximally-specific non-abstract method, JVMS 5.4.3.3 step 3a /
    // 5.4.3.4 step 5 -- else the first ABSTRACT declaration in its
    // transitive-interface order (defaults skipped). The walk used to return
    // its first LIFO find: `abstract class Base implements Shape {}` /
    // `class Leaf extends Base implements Rounded {}` (`Rounded extends Shape`
    // re-declaring `area` as a default) met `Shape.area` first and the
    // `MemberName` took its ACC_ABSTRACT (`revealDirect(..).getModifiers()`
    // 1025, HotSpot 1); `interface Both extends Left, Right` of two abstract
    // `m`s named `Right` (HotSpot `Left`).
    let mut candidates: Vec<(ClassId, u16)> = Vec::new();
    while let Some(iface) = pending.pop() {
        if seen.contains(&iface) {
            continue;
        }
        if seen.len() >= 256 {
            if !candidates.is_empty() {
                break;
            }
            return (MhnVerdict::Unknown, None);
        }
        seen.push(iface);
        // A compatibility-stub interface declares nothing we can see (its
        // members live in the native registry): no evidence.
        match ctx.class_name_of_id(iface) {
            Some(iface_name) if mhn_class_chain_has_stub(ctx, &iface_name) => {
                if !candidates.is_empty() {
                    break;
                }
                return (MhnVerdict::Unknown, None);
            }
            Some(_) => {}
            None => {
                if !candidates.is_empty() {
                    break;
                }
                return (MhnVerdict::Unknown, None);
            }
        }
        let (methods, declares_any) = mhn_declared_methods_named(ctx, iface, name);
        enumerated_any |= declares_any;
        if let Some(m) = methods.iter().find(|m| {
            m.name == name
                && m.descriptor == desc
                && m.access_flags & (MHN_ACC_STATIC | MHN_ACC_PRIVATE) == 0
        }) {
            candidates.push((iface, m.access_flags));
        }
        pending.extend(ctx.class_interfaces(iface));
    }
    if let Some(&first) = candidates.first() {
        let (iface, flags) = if candidates.len() == 1 {
            first
        } else {
            mhn_maximally_specific_default(ctx, &candidates)
                .or_else(|| mhn_first_abstract_in_hotspot_order(ctx, class_id, &candidates))
                .unwrap_or(first)
        };
        return (MhnVerdict::Found(flags), Some(iface));
    }
    if enumerated_any {
        (MhnVerdict::Absent, None)
    } else {
        (MhnVerdict::Unknown, None)
    }
}

/// Among the superinterface declarations `candidates` (interface, flags) a
/// resolution collected, the one maximally-specific NON-abstract method
/// (JVMS 5.4.3.3 step 3a; what HotSpot's `default_methods` holds): no other
/// candidate's interface extends its interface. `None` when there is not
/// exactly one (re-abstracted: the first abstract in HotSpot's order wins;
/// conflicting defaults: the caller keeps the first find). Interpreter round
/// i5 wave 104, lane L4.
pub(crate) fn mhn_maximally_specific_default(
    ctx: &dyn NativeContext,
    candidates: &[(ClassId, u16)],
) -> Option<(ClassId, u16)> {
    let maximal: Vec<(ClassId, u16)> = candidates
        .iter()
        .copied()
        .filter(|&(iface, _)| {
            !candidates
                .iter()
                .any(|&(other, _)| other != iface && mhn_interface_extends(ctx, other, iface))
        })
        .collect();
    let mut concrete = maximal
        .iter()
        .copied()
        .filter(|&(_, flags)| flags & MHN_ACC_ABSTRACT == 0);
    let one = concrete.next()?;
    concrete.next().is_none().then_some(one)
}

/// Does interface `sub` extend `sup`, directly or transitively? Bounded like
/// the resolution walk. Interpreter round i5 wave 104, lane L4.
pub(crate) fn mhn_interface_extends(ctx: &dyn NativeContext, sub: ClassId, sup: ClassId) -> bool {
    let mut pending = ctx.class_interfaces(sub);
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(iface) = pending.pop() {
        if iface == sup {
            return true;
        }
        if seen.contains(&iface) {
            continue;
        }
        if seen.len() >= 256 {
            return false;
        }
        seen.push(iface);
        pending.extend(ctx.class_interfaces(iface));
    }
    false
}

/// The first ABSTRACT declaration among `candidates` in HotSpot's
/// transitive-interface order of `class_id` (what
/// `InstanceKlass::lookup_method_in_all_interfaces` with defaults skipped
/// answers), or `None`. Interpreter round i5 wave 104, lane L4.
pub(crate) fn mhn_first_abstract_in_hotspot_order(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    candidates: &[(ClassId, u16)],
) -> Option<(ClassId, u16)> {
    let mut order: Vec<ClassId> = Vec::new();
    let mut expanded: Vec<ClassId> = Vec::new();
    mhn_hotspot_transitive_interfaces_into(ctx, class_id, 0, &mut expanded, &mut order);
    order.iter().find_map(|iface| {
        candidates
            .iter()
            .find(|&&(c, flags)| c == *iface && flags & MHN_ACC_ABSTRACT != 0)
            .copied()
    })
}

/// `ClassFileParser::compute_transitive_interfaces`' order for `cid`,
/// appended to `out` without duplicates: the superclass's transitive
/// interfaces, then each local interface's, then the local interfaces
/// themselves. `expanded` keeps a class from being expanded twice (its
/// interfaces are already in `out`); bounded like the resolution walk.
pub(crate) fn mhn_hotspot_transitive_interfaces_into(
    ctx: &dyn NativeContext,
    cid: ClassId,
    depth: usize,
    expanded: &mut Vec<ClassId>,
    out: &mut Vec<ClassId>,
) {
    if depth > 64 || out.len() >= 256 || expanded.contains(&cid) {
        return;
    }
    expanded.push(cid);
    if let Some(sup) = ctx.superclass_of(cid).filter(|&sup| sup != cid) {
        mhn_hotspot_transitive_interfaces_into(ctx, sup, depth + 1, expanded, out);
    }
    let locals = ctx.class_interfaces(cid);
    for &local in &locals {
        mhn_hotspot_transitive_interfaces_into(ctx, local, depth + 1, expanded, out);
    }
    for local in locals {
        if !out.contains(&local) {
            out.push(local);
        }
    }
}

/// JVMS 5.4.3.2 field resolution of `name` with descriptor `desc` from
/// `class_id`: the class's own field, then its direct superinterfaces
/// (recursively), then its superclass (recursively). The found field's flags
/// matter only for static-ness, which HotSpot reports through the reference
/// kind.
pub(crate) fn mhn_field_verdict(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> MhnVerdict {
    match mhn_field_resolve(ctx, class_id, name, desc) {
        None => MhnVerdict::Unknown,
        Some(None) => MhnVerdict::Absent,
        Some(Some((flags, _))) => MhnVerdict::Found(flags),
    }
}

/// [`mhn_field_verdict`] with the DECLARING class of the field it finds
/// (`Lookup.find*`'s refusal texts name it): `None` is unknown, `Some(None)`
/// absent, `Some(Some((flags, declaring)))` found.
pub(crate) fn mhn_field_resolve(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> Option<Option<(u16, ClassId)>> {
    let mut enumerated_any = false;
    let mut seen: Vec<ClassId> = Vec::new();
    match mhn_field_search(ctx, class_id, name, desc, 0, &mut seen, &mut enumerated_any) {
        Err(()) => None,
        Ok(Some(found)) => Some(Some(found)),
        // Absent, also when no class on the path declares any field at all
        // (an interface such as `Runnable`, `Object`, an empty class):
        // interpreter round i1 wave 49, lane L4, probe
        // `L4W49FieldlessClassMissingField`. That case answered "unknown"
        // (`enumerated_any` false), and the `Lookup` field finders linked a
        // handle to a field that does not exist.
        Ok(None) => {
            // A compatibility stub declares no fields we can see, so a stub
            // anywhere on the searched path is no evidence of absence.
            let any_stub = seen.iter().any(|&cid| match ctx.class_name_of_id(cid) {
                Some(n) => mhn_class_chain_has_stub(ctx, &n),
                None => true,
            });
            if any_stub {
                None
            } else {
                Some(None)
            }
        }
    }
}

/// Whether the loaded class `class_name` or one of its superclasses is a
/// compatibility stub, without loading anything: `method_exists` answers
/// `true` for ANY member of a stub on that chain (its members live in the
/// native registry, not a class file), and no class declares — nor does the
/// registry hold — a method named with the `<`…`>` spelling below.
pub(crate) fn mhn_class_chain_has_stub(ctx: &dyn NativeContext, class_name: &str) -> bool {
    ctx.method_exists(class_name, "<cratonvm-stub-probe>", "()V")
}

/// One step of [`mhn_field_verdict`]; `Err(())` gives up (too deep or wide).
pub(crate) fn mhn_field_search(
    ctx: &dyn NativeContext,
    cid: ClassId,
    name: &str,
    desc: &str,
    depth: usize,
    seen: &mut Vec<ClassId>,
    enumerated_any: &mut bool,
) -> Result<Option<(u16, ClassId)>, ()> {
    if depth > 64 || seen.len() > 320 {
        return Err(());
    }
    if seen.contains(&cid) {
        // Already searched through another path (an interface reached twice).
        return Ok(None);
    }
    seen.push(cid);
    let (fields, declares_any) = mhn_declared_fields_named(ctx, cid, name);
    *enumerated_any |= declares_any;
    if let Some(f) = fields
        .iter()
        .find(|f| f.name == name && f.descriptor == desc)
    {
        return Ok(Some((f.access_flags, cid)));
    }
    for iface in ctx.class_interfaces(cid) {
        if let Some(found) =
            mhn_field_search(ctx, iface, name, desc, depth + 1, seen, enumerated_any)?
        {
            return Ok(Some(found));
        }
    }
    match ctx.superclass_of(cid) {
        Some(sup) if sup != cid => {
            mhn_field_search(ctx, sup, name, desc, depth + 1, seen, enumerated_any)
        }
        _ => Ok(None),
    }
}

/// `MethodHandleNatives.init(MemberName self, Object ref)`
///
/// Initializes a MemberName from a reflected member (Method, Field, Constructor).
/// Copies the declaring class, name, type, and flags from the reflected object
/// into the MemberName fields.
pub(crate) fn native_mhn_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // MemberName layout (JDK):
    //   clazz  (Class<?>)
    //   name   (String)
    //   type   (Object, MethodType or Class for fields)
    //   flags  (int, refKind | kind_bits | ACC_*)
    // The `clazz`, `flags` (and for Method/Constructor `type`) must be
    // populated by this native — the Java-side MemberName(Field)/
    // MemberName(Method)/MemberName(Constructor) constructors only set
    // `name` and `type` themselves and rely on init() for the rest.
    //
    // MemberName kind bits (see MemberName constants):
    const IS_METHOD: i32 = 0x1_0000;
    const IS_CONSTRUCTOR: i32 = 0x2_0000;
    const IS_FIELD: i32 = 0x4_0000;
    // Reference kinds per JVM spec table 5.4.3.5:
    const REF_GET_FIELD: i32 = 1;
    const REF_GET_STATIC: i32 = 2;
    const REF_INVOKE_VIRTUAL: i32 = 5;
    const REF_INVOKE_STATIC: i32 = 6;
    const REF_INVOKE_SPECIAL: i32 = 7;
    const REF_NEW_INVOKE_SPECIAL: i32 = 8;
    const REF_INVOKE_INTERFACE: i32 = 9;
    const ACC_STATIC: i32 = 0x0008;

    let member_name = crate::obj_arg(args, 0)?;
    let ref_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };

    // Discriminate by the reflected object's class name.
    let ref_class_id = ctx.class_id_of_object(ref_obj);
    let ref_class_name = ctx.class_name_of_id(ref_class_id).unwrap_or_default();

    // GC-safety: the Constructor branch below calls `create_string`/
    // `new_array`/`primitive_class_mirror`/`alloc_concurrent_synthetic`,
    // any of which can trigger a collection that relocates `member_name`/
    // `ref_obj` (both captured above, well before the match). Pin them for
    // the whole match; the Constructor branch re-reads through the pins
    // before each of its own risky re-uses, and this final re-read (right
    // before the trailing `set_field` below) corrects `member_name`
    // regardless of which branch ran or how many times it was internally
    // re-read (a Rust `let` shadow inside one match arm does not persist
    // past that arm, so the outer binding needs its own final refresh).
    let member_name_pin = ctx.pin_native_root(member_name);
    let ref_obj_pin = ctx.pin_native_root(ref_obj);
    match ref_class_name.as_str() {
        "java/lang/reflect/Field" => {
            // Copy clazz, name, type from the Field; compute flags.
            let clazz = ctx.get_field_by_name(ref_obj, "clazz");
            let name = ctx.get_field_by_name(ref_obj, "name");
            let ty = ctx.get_field_by_name(ref_obj, "type");
            let modifiers = match ctx.get_field_by_name(ref_obj, "modifiers") {
                Value::Int(v) => v,
                _ => 0,
            };
            let ref_kind = if (modifiers & ACC_STATIC) != 0 {
                REF_GET_STATIC
            } else {
                REF_GET_FIELD
            };
            let mut flags = IS_FIELD | (modifiers & 0xFFFF) | (ref_kind << 24);
            // HotSpot's `init_field_MemberName` sets `TRUSTED_FINAL` (wave 80,
            // lane L4): `unreflectSetter` of an accessible trusted final then
            // refuses ("final field has no write access").
            if mhn_init_field_trusted_final(ctx, ref_obj, modifiers) {
                flags |= MN_TRUSTED_FINAL;
            }

            mn_set(ctx, member_name, "clazz", MN_CLAZZ, clazz);
            mn_set(ctx, member_name, "name", MN_NAME, name);
            mn_set(ctx, member_name, "type", MN_TYPE, ty);
            mn_set(ctx, member_name, "flags", MN_FLAGS, Value::Int(flags));
        }
        "java/lang/reflect/Method" => {
            let clazz = ctx.get_field_by_name(ref_obj, "clazz");
            let name = ctx.get_field_by_name(ref_obj, "name");
            let modifiers = match ctx.get_field_by_name(ref_obj, "modifiers") {
                Value::Int(v) => v,
                _ => 0,
            };
            let is_static = (modifiers & ACC_STATIC) != 0;
            // HotSpot's `init_method_MemberName`: a direct (not vtable) call --
            // a private method, or one that is final / in a final class -- is
            // `REF_invokeSpecial` ("a direct call, not necessarily an
            // invokespecial"). `Lookup.unreflect`'s refusal text names the kind
            // (SBR3 lane mh).
            let direct_call = (modifiers & (0x0002 | 0x0010)) != 0 || {
                let clazz_mirror = ctx.get_field_by_name(ref_obj, "clazz");
                match clazz_mirror {
                    Value::Object(Some(m)) => mirror_class_id(&*ctx, m)
                        .is_some_and(|cid| (ctx.class_access_flags(cid) & 0x0010) != 0),
                    _ => false,
                }
            };
            // An instance method of an INTERFACE that is not a direct call is
            // HotSpot's itable call, `REF_invokeInterface` (`CallInfo`'s
            // `itable_call`; a default method too): `DirectMethodHandle.make`
            // then builds a `$Interface`, whose linker checks the receiver
            // (interpreter round i2 wave 55, lane L4; `L4W55LinkToFromBytecode`
            // row `iface-class`). It was `REF_invokeVirtual`, a plain handle.
            let in_interface = !is_static
                && !direct_call
                && match ctx.get_field_by_name(ref_obj, "clazz") {
                    Value::Object(Some(m)) => mirror_class_id(&*ctx, m)
                        .is_some_and(|cid| (ctx.class_access_flags(cid) & 0x0200) != 0),
                    _ => false,
                };
            let ref_kind = if is_static {
                REF_INVOKE_STATIC
            } else if direct_call {
                REF_INVOKE_SPECIAL
            } else if in_interface {
                REF_INVOKE_INTERFACE
            } else {
                REF_INVOKE_VIRTUAL
            };
            let flags = IS_METHOD | (modifiers & 0xFFFF) | (ref_kind << 24);
            // Interpreter round i4 wave 95, lane L4: `init_method_MemberName`'s
            // `MN_CALLER_SENSITIVE` (see `mhn_method_is_caller_sensitive`), so
            // `Lookup.unreflect` binds a caller-sensitive method's caller too.
            let caller_sensitive = match (clazz, name) {
                (Value::Object(Some(m)), Value::Object(Some(n))) => {
                    match (mirror_class_id(&*ctx, m), ctx.read_string(n)) {
                        (Some(holder), Some(method_name)) => {
                            let descriptor =
                                crate::lang_reflect::executable_descriptor(&*ctx, ref_obj);
                            mhn_method_is_caller_sensitive(
                                &*ctx,
                                holder,
                                &method_name,
                                (!descriptor.is_empty()).then_some(descriptor.as_str()),
                            )
                        }
                        _ => false,
                    }
                }
                _ => false,
            };
            let flags = if caller_sensitive {
                flags | MN_CALLER_SENSITIVE
            } else {
                flags
            };

            mn_set(ctx, member_name, "clazz", MN_CLAZZ, clazz);
            mn_set(ctx, member_name, "name", MN_NAME, name);
            // `type` (MethodType) is populated by the Java constructor
            // (`invokevirtual Method.getGenericReturnType` etc.); leave as-is.
            mn_set(ctx, member_name, "flags", MN_FLAGS, Value::Int(flags));
        }
        "java/lang/reflect/Constructor" => {
            let clazz = ctx.get_field_by_name(ref_obj, "clazz");
            let modifiers = match ctx.get_field_by_name(ref_obj, "modifiers") {
                Value::Int(v) => v,
                _ => 0,
            };
            let flags = IS_CONSTRUCTOR | (modifiers & 0xFFFF) | (REF_NEW_INVOKE_SPECIAL << 24);
            let _ = REF_INVOKE_SPECIAL;

            mn_set(ctx, member_name, "clazz", MN_CLAZZ, clazz);
            // name = "<init>" — the constructor's name
            let name_str = ctx.create_string("<init>");
            let member_name = ctx.read_native_pin(member_name_pin, member_name);
            mn_set(
                ctx,
                member_name,
                "name",
                MN_NAME,
                Value::Object(Some(name_str)),
            );
            mn_set(ctx, member_name, "flags", MN_FLAGS, Value::Int(flags));

            // Interpreter round i6 wave 124, lane L4
            // (`CRATONVM_MHN_INIT_CONSTRUCTOR_JDK_TYPE`, default ON, parity; page
            // `i88-L3-proposal-first-use-reflective-constructor-cost`): on a REAL
            // `MemberName`, leave `type` unset, as HotSpot's
            // `init_method_MemberName` does. The JDK constructor then stores
            // `{void.class, ptypes}` and `getMethodType()` interns it (the
            // `--compatible` natives convert that shape through the JDK body,
            // `mn_type_slot_converted`). The minted type below is not interned,
            // and its private form claims it as its own basic and erased type, so
            // `revealDirect(unreflectConstructor(c)).getMethodType().erase()` kept
            // reference parameters and `==` against `MethodType.methodType(..)`
            // was false. The fabricated layout (no JDK constructor) keeps it.
            let member_name = ctx.read_native_pin(member_name_pin, member_name);
            if crate::nbflags().mhn_init_constructor_jdk_type
                && !mn_has_synthetic_layout(ctx, member_name)
            {
                let member_name = ctx.read_native_pin(member_name_pin, member_name);
                ctx.unpin_native_roots(member_name_pin);
                mn_set_vmindex(ctx, member_name, 1);
                return Ok(None);
            }

            // Populate `type` (the MethodType, MemberName slot 2). Unlike the
            // Method case — where the Java `MemberName(Method)` constructor
            // fills `type` itself — the constructor path leaves it unset, so
            // `MemberName.getMethodType()` (which returns slot 2 directly)
            // would hand back an uninitialized slot. Real JDK callers then do
            // `getMethodType().changeReturnType(...)` / `.returnType()` on it;
            // reading slot 0 of a garbage "MethodType" dereferences a wild
            // pointer and SIGSEGVs. This is exactly the crash hit by
            // `ReflectionFactory.newConstructorForSerialization` →
            // `DirectMethodHandle.makeAllocator` (and thus any JUnit 4 run,
            // whose RunNotifier builds serializable constructors).
            //
            // A constructor's invocation type is `(paramTypes...)void`, so
            // build that MethodType from the reflected Constructor's
            // `parameterTypes` and a void return mirror.
            let ref_obj = ctx.read_native_pin(ref_obj_pin, ref_obj);
            let ptypes_ref = match ctx.get_field_by_name(ref_obj, "parameterTypes") {
                Value::Object(Some(a)) => a,
                // `MethodType.ptypes` is a `Class[]` (wave 45).
                _ => new_class_array(ctx, 0),
            };
            // GC-safety: `primitive_class_mirror`/`alloc_concurrent_synthetic`
            // below can trigger a collection that relocates `ptypes_ref`
            // (embedded into the new MethodType only after both run); pin it
            // too (same batch as `void_mirror`).
            let ptypes_pin = ctx.pin_native_root(ptypes_ref);
            let void_mirror = ctx.primitive_class_mirror(NAME_VOID);
            let void_mirror_pin = ctx.pin_native_root(void_mirror);
            let mt: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6)?;
            let void_mirror = ctx.read_native_pin(void_mirror_pin, void_mirror);
            let ptypes_ref = ctx.read_native_pin(ptypes_pin, ptypes_ref);
            ctx.unpin_native_roots(ptypes_pin);
            ctx.set_field(mt, 0, Value::Object(Some(void_mirror)));
            ctx.set_field(mt, 1, Value::Object(Some(ptypes_ref)));
            // gen r4w3/rooting: refreshed in place across the form allocation.
            let mut mt = mt;
            populate_method_type_form(ctx, &mut mt)?;
            let member_name = ctx.read_native_pin(member_name_pin, member_name);
            mn_set(ctx, member_name, "type", MN_TYPE, Value::Object(Some(mt)));
        }
        _ => {
            // Unknown ref object — bail quietly.
        }
    }

    // Mark resolved. Re-read `member_name` once more (see the pin-setup
    // comment above the match): whichever branch ran, this is the
    // authoritative final refresh.
    let member_name = ctx.read_native_pin(member_name_pin, member_name);
    ctx.unpin_native_roots(member_name_pin);
    mn_set_vmindex(ctx, member_name, 1);
    Ok(None)
}

// `native_mhn_link_method` (`MethodHandleNatives.linkMethod`) was deleted in
// interpreter round i4 wave 85 (lane L4) with its registration: JDK 25's
// `linkMethod` is Java (the VM's upcall for an invokehandle site), no code in
// this VM calls it, its ambient `Bridge` kind lost to that bytecode in both
// modes, and it answered a synthetic MethodHandle where a `MemberName` is
// returned. See `native-builtins/src/lib.rs`, MethodHandleNatives section.

/// `MethodHandleNatives.objectFieldOffset(MemberName self)`
///
/// Returns the field offset for Unsafe-style access. We return the field
/// index directly since our field layout is index-based not byte-offset-based.
pub(crate) fn native_mhn_object_field_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // JIT round 20 wave 8c (lane huntDisp): the slot, as the `Unsafe` offset
    // this VM answers (`unsafe_encode_field_offset`: a byte offset under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`, the slot itself otherwise). The
    // JDK hands it straight to `Unsafe` (`DirectMethodHandle$Accessor`,
    // `VarHandle*$FieldInstance*`), whose natives decode it.
    let answer = |ctx: &dyn NativeContext, slot: i64| -> MethodCallResult {
        let offset = if slot >= 0 {
            // Cast: a non-negative slot / offset.
            crate::unsafe_natives_ext::unsafe_encode_field_offset(
                ctx.unsafe_byte_field_offsets(),
                slot as usize,
            ) as i64
        } else {
            slot
        };
        Ok(Some(Value::Long(offset)))
    };
    let member_name = crate::obj_arg(args, 0)?;
    // Wave 73 (`CRATONVM_MHN_RESOLVE_MEMO`): a found slot, remembered.
    if mhn_resolve_memo_enabled() {
        if let Some(slot) = mhn_object_field_offset_memo(ctx, member_name) {
            return answer(&*ctx, slot);
        }
    }
    // A REAL `MemberName` has no vmindex slot (`mn_get_vmindex` answers 0), so
    // every instance field of a real `Lookup.findVarHandle` /
    // `unreflectGetter` handle got offset 0 -- the first slot, whatever the
    // field. Its slot is the declared field's own (SBR2 wave 10, PUREVH).
    if !mn_has_synthetic_layout(ctx, member_name) {
        if let Some((class_id, _class_name, field_name)) = mhn_static_member(ctx, args) {
            // `declared_fields_with_aliases`, not `declared_fields`: a lambda
            // proxy is no class-store class, so only the alias path knows its
            // captured `arg$N` fields (`Field.get` of a lambda capture, which
            // AssertJ's `extracting("...arg$1")` reads, answered `null`).
            // Then up the superclass chain (wave 79, lane L4): a `MemberName`
            // still naming a subclass of the field's holder found nothing
            // and answered slot 0.
            if let Some((_, slot)) = mhn_instance_field_holder_from(&*ctx, class_id, &field_name) {
                return answer(&*ctx, slot as i64);
            }
        } else if let Some(slot) = mhn_lambda_proxy_field_slot(ctx, member_name) {
            // A lambda proxy's capture (wave 80): no class-store name.
            return answer(&*ctx, slot as i64);
        }
    }
    let vmindex = mn_get_vmindex(ctx, member_name) as i64;
    answer(&*ctx, vmindex)
}

/// [`native_mhn_object_field_offset`]'s answer for a real `MemberName` whose
/// instance field it finds, from [`MHN_MEMO`] or computed the same way and
/// remembered; `None` (the caller's ordinary path runs) for anything else.
pub(crate) fn mhn_object_field_offset_memo(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<i64> {
    let layout = mn_layout(ctx, mn);
    if layout.synthetic {
        return None;
    }
    let Value::Object(Some(clazz)) = ctx.get_field(mn, layout.clazz) else {
        return None;
    };
    let Value::Object(Some(name)) = ctx.get_field(mn, layout.name) else {
        return None;
    };
    let epoch = ctx.gc_address_epoch();
    let key = [mhn_obj_addr(clazz), mhn_obj_addr(name), 0];
    if let Some(MhnMemoAnswer::Offset(slot)) =
        mhn_memo_get(&*ctx, MhnMemoKind::ObjectOffset, key, 0)
    {
        return Some(slot);
    }
    // `mhn_static_member`'s reads (its class-name check included), then the
    // ordinary path's search. Pure reads: no GC point.
    let class_id = ctx.class_id_from_mirror(clazz)?;
    ctx.class_name_arc_of_id(class_id)?;
    let field_name = ctx.read_string(name)?;
    // The ordinary path's search, superclasses included (wave 79).
    let (_, slot) = mhn_instance_field_holder_from(&*ctx, class_id, &field_name)?;
    let slot = slot as i64;
    mhn_memo_put(
        &*ctx,
        epoch,
        MhnMemoKind::ObjectOffset,
        key,
        0,
        MhnMemoAnswer::Offset(slot),
    );
    Some(slot)
}

/// The static-field key of `mn` under [`mhn_resolve_memo_enabled`]: what
/// [`mn_static_field`] reads (`flags` of a static kind, `clazz`, `name`).
pub(crate) fn mhn_static_field_memo_key(
    ctx: &mut dyn NativeContext,
    mn: ObjectRef,
) -> Option<([usize; 3], i32, u64)> {
    let layout = mn_layout(ctx, mn);
    let Value::Int(flags) = ctx.get_field(mn, layout.flags) else {
        return None;
    };
    if !matches!((flags >> 24) & 0x0F, 2 | 4) {
        return None;
    }
    let Value::Object(Some(clazz)) = ctx.get_field(mn, layout.clazz) else {
        return None;
    };
    let Value::Object(Some(name)) = ctx.get_field(mn, layout.name) else {
        return None;
    };
    Some((
        [mhn_obj_addr(clazz), mhn_obj_addr(name), 0],
        flags,
        ctx.gc_address_epoch(),
    ))
}

/// [`native_mhn_static_field_offset`]'s answer for a static field it finds,
/// remembered under [`mhn_resolve_memo_enabled`]. A hit still registers the
/// offset for its field, as every call of the ordinary path does.
pub(crate) fn mhn_static_field_offset_memo(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<i64> {
    let (key, flags, epoch) = mhn_static_field_memo_key(ctx, mn)?;
    if let Some(MhnMemoAnswer::StaticOffset {
        offset,
        class_id,
        field_index,
    }) = mhn_memo_get(&*ctx, MhnMemoKind::StaticOffset, key, flags)
    {
        crate::unsafe_natives_ext::remember_unsafe_static_field_offset(
            ctx.vm_identity(),
            offset as usize,
            class_id,
            field_index,
        );
        // JIT round 20 wave 11: the scaled static byte offset under
        // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (the minted one otherwise).
        return Some(crate::unsafe_natives_ext::unsafe_static_offset_answer(
            &*ctx,
            offset as usize,
            class_id,
            field_index,
        ) as i64);
    }
    let (class_id, name) = mn_static_field(ctx, mn)?;
    let field_index = ctx.static_field_index_by_name(class_id, &name)?;
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let offset = crate::register_static_field_offset_in_vm(
        ctx.vm_identity(),
        &class_name,
        &name,
        class_id,
        field_index,
    );
    mhn_memo_put(
        &*ctx,
        epoch,
        MhnMemoKind::StaticOffset,
        key,
        flags,
        MhnMemoAnswer::StaticOffset {
            offset,
            class_id,
            field_index,
        },
    );
    // JIT round 20 wave 11: as the hit above.
    Some(crate::unsafe_natives_ext::unsafe_static_offset_answer(
        &*ctx,
        offset as usize,
        class_id,
        field_index,
    ) as i64)
}

/// [`native_mhn_static_field_base`]'s declaring class, remembered under
/// [`mhn_resolve_memo_enabled`].
pub(crate) fn mhn_static_field_base_memo(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<ClassId> {
    let (key, flags, epoch) = mhn_static_field_memo_key(ctx, mn)?;
    if let Some(MhnMemoAnswer::StaticBase(class_id)) =
        mhn_memo_get(&*ctx, MhnMemoKind::StaticBase, key, flags)
    {
        return Some(class_id);
    }
    let (class_id, _) = mn_static_field(ctx, mn)?;
    mhn_memo_put(
        &*ctx,
        epoch,
        MhnMemoKind::StaticBase,
        key,
        flags,
        MhnMemoAnswer::StaticBase(class_id),
    );
    Some(class_id)
}

/// The declaring class and name of a resolved static-field `MemberName`
/// (reference kind `REF_getStatic` / `REF_putStatic`), or `None`.
///
/// gci i40/fg (`gcg-g4oo` item 5, Batch 9b R3): a STATIC-kind member this
/// cannot read is the producer of `--compatible`'s `Field.get(null)` refusal
/// (`Unsafe access with a null base and an offset that is neither ... : 0x10`):
/// both [`native_mhn_static_field_base`] (`null`) and
/// [`native_mhn_static_field_offset`] (the instance fallback's vmindex `0`,
/// encoded `0x10` under byte field offsets) answer from here, so the JDK's
/// `DirectMethodHandle$StaticAccessor` gets a null `staticBase`. Each read is
/// now taken separately, and a miss is reported under
/// `CRATONVM_DBG_MHN_STATIC_FIELD_MISS` ([`note_mn_static_field_miss`]) with
/// the step that failed. The answer is unchanged: every read is a pure read.
pub(crate) fn mn_static_field(ctx: &mut dyn NativeContext, mn: ObjectRef) -> Option<(ClassId, String)> {
    let flags = match mn_get(ctx, mn, "flags", MN_FLAGS) {
        Value::Int(f) => f,
        _ => return None,
    };
    let ref_kind = (flags >> 24) & 0x0F;
    if ref_kind != 2 && ref_kind != 4 {
        return None;
    }
    let mirror = match mn_get(ctx, mn, "clazz", MN_CLAZZ) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let name_obj = match mn_get(ctx, mn, "name", MN_NAME) {
        Value::Object(Some(n)) => Some(n),
        _ => None,
    };
    let class_id = mirror.and_then(|m| ctx.class_id_from_mirror(m));
    // gci i41/rv: decode `name` only once the mirror resolved, as before
    // i40/fg. `read_string` follows the String's `value` array, the one read
    // here that dereferences heap memory past the `MemberName` itself (the
    // suspected `name-unreadable` producer is a String whose storage was
    // freed), and the miss rule never asks `name_read` before `class_known`
    // ([`mn_static_field_miss`]), so the token is the same.
    let name = if class_id.is_some() {
        name_obj.and_then(|n| ctx.read_string(n))
    } else {
        None
    };
    let (class_known, name_read) = (class_id.is_some(), name.is_some());
    let (Some(class_id), Some(name)) = (class_id, name) else {
        let miss = mn_static_field_miss(
            mirror.is_some(),
            class_known,
            name_obj.is_some(),
            name_read,
        );
        note_mn_static_field_miss(&*ctx, flags, miss, mirror, name_obj);
        return None;
    };
    // The class that DECLARES the field (JVMS 5.4.3.2: the class, its
    // superinterfaces, its superclasses): `clazz` is the class the finder was
    // asked on, so `findStaticGetter(Sub.class, "K", int.class)` of an
    // interface constant `I.K` had no static slot on `Sub`, and its offset fell
    // back to the vmindex no Unsafe access resolves -- `UnsupportedOperationException:
    // real static field MethodHandle: unregistered offset` under `--jdk-only`
    // where HotSpot reads the constant (interpreter round i2 wave 62, lane L4;
    // `L4W24LookupFinderMessages` row `findStaticGetter interface constant via
    // subclass`). The offset and the base both come from here, so they agree.
    let class_id = static_field_holder(&*ctx, class_id, &name).unwrap_or(class_id);
    Some((class_id, name))
}

/// The read [`mn_static_field`] could not make for a static-kind `MemberName`
/// (gci i40/fg), first failing step first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MnStaticFieldMiss {
    /// `clazz` is null.
    NoClazz,
    /// `name` is null.
    NoName,
    /// `clazz` is not a mirror `class_id_from_mirror` maps (the reverse
    /// mirror map has no row for that address).
    MirrorUnmapped,
    /// `name` does not decode as a `String` (`read_string` answered `None`).
    NameUnreadable,
}

impl MnStaticFieldMiss {
    pub(crate) fn token(self) -> &'static str {
        match self {
            MnStaticFieldMiss::NoClazz => "no-clazz",
            MnStaticFieldMiss::NoName => "no-name",
            MnStaticFieldMiss::MirrorUnmapped => "mirror-unmapped",
            MnStaticFieldMiss::NameUnreadable => "name-unreadable",
        }
    }
}

/// The pure rule behind [`MnStaticFieldMiss`]: `None` when every read
/// succeeded. A null slot is named before an unreadable one, `clazz` before
/// `name`.
pub(crate) fn mn_static_field_miss(
    has_clazz: bool,
    class_known: bool,
    has_name: bool,
    name_read: bool,
) -> Option<MnStaticFieldMiss> {
    if !has_clazz {
        Some(MnStaticFieldMiss::NoClazz)
    } else if !has_name {
        Some(MnStaticFieldMiss::NoName)
    } else if !class_known {
        Some(MnStaticFieldMiss::MirrorUnmapped)
    } else if !name_read {
        Some(MnStaticFieldMiss::NameUnreadable)
    } else {
        None
    }
}

/// `CRATONVM_DBG_MHN_STATIC_FIELD_MISS` (gci i40/fg, diagnostic, default OFF):
/// one stderr line per static-kind `MemberName` [`mn_static_field`] could not
/// read, naming the step ([`MnStaticFieldMiss`]), the mirror's class by the
/// slow lookups (`mirror_class_id`, `mirror_class_name`), the `name` object's
/// class and heap kind, and the collection epoch. Read only on a miss (cold);
/// pure reads, no allocation.
#[cold]
#[inline(never)]
pub(crate) fn note_mn_static_field_miss(
    ctx: &dyn NativeContext,
    flags: i32,
    miss: Option<MnStaticFieldMiss>,
    mirror: Option<ObjectRef>,
    name_obj: Option<ObjectRef>,
) {
    if !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MHN_STATIC_FIELD_MISS") {
        return;
    }
    let mirror_text = match mirror {
        Some(m) => format!(
            "{:#x} slow-cid={:?} slow-name={:?}",
            m.as_ptr() as usize,
            crate::lang_class::mirror_class_id(ctx, m).map(|c| c.as_u32()),
            crate::lang_class::mirror_class_name(ctx, m),
        ),
        None => "null".to_string(),
    };
    let name_text = match name_obj {
        Some(n) => {
            let cid = ctx.class_id_of_object(n);
            format!(
                "{:#x} class={:?} kind={:?}",
                n.as_ptr() as usize,
                ctx.class_name_of_id(cid),
                ctx.heap_kind_of(n),
            )
        }
        None => "null".to_string(),
    };
    eprintln!(
        "[mhn-static-field] miss={} flags={:#x} epoch={} clazz={} name={}",
        miss.map_or("none", MnStaticFieldMiss::token),
        flags,
        ctx.gc_address_epoch(),
        mirror_text,
        name_text,
    );
}

#[cfg(test)]
mod gci_i40fg_mhn_static_field_miss_tests {
    use super::*;

    #[test]
    fn gci_i40fg_every_read_made_is_no_miss() {
        assert_eq!(mn_static_field_miss(true, true, true, true), None);
    }

    #[test]
    fn gci_i40fg_the_first_failing_step_is_named() {
        // A null slot before an unreadable one, `clazz` before `name`.
        assert_eq!(
            mn_static_field_miss(false, false, false, false),
            Some(MnStaticFieldMiss::NoClazz)
        );
        assert_eq!(
            mn_static_field_miss(true, false, false, false),
            Some(MnStaticFieldMiss::NoName)
        );
        assert_eq!(
            mn_static_field_miss(true, false, true, false),
            Some(MnStaticFieldMiss::MirrorUnmapped)
        );
        assert_eq!(
            mn_static_field_miss(true, false, true, true),
            Some(MnStaticFieldMiss::MirrorUnmapped)
        );
        assert_eq!(
            mn_static_field_miss(true, true, true, false),
            Some(MnStaticFieldMiss::NameUnreadable)
        );
    }

    /// gci i41/rv: `mn_static_field` decodes `name` only after the mirror
    /// resolved, which is exact because the rule never asks `name_read` while
    /// `class_known` is false (or a slot is null): every such answer is the
    /// same with the name read or not.
    #[test]
    fn gci_i41rv_name_read_is_not_asked_before_the_mirror() {
        for has_clazz in [false, true] {
            for has_name in [false, true] {
                for class_known in [false, true] {
                    if has_clazz && has_name && class_known {
                        continue;
                    }
                    assert_eq!(
                        mn_static_field_miss(has_clazz, class_known, has_name, false),
                        mn_static_field_miss(has_clazz, class_known, has_name, true),
                        "{has_clazz} {class_known} {has_name}"
                    );
                }
            }
        }
        let src = LANG_INVOKE_SRC.replace("\r\n", "\n");
        assert!(src.contains(concat!(
            "    let name = if class_id.is_some() \u{7b}\n",
            "        name_obj.and_then(|n| ctx.read_string(n))\n"
        )));
    }

    #[test]
    fn gci_i40fg_each_miss_has_its_own_token() {
        let tokens = [
            MnStaticFieldMiss::NoClazz.token(),
            MnStaticFieldMiss::NoName.token(),
            MnStaticFieldMiss::MirrorUnmapped.token(),
            MnStaticFieldMiss::NameUnreadable.token(),
        ];
        for (i, a) in tokens.iter().enumerate() {
            for b in &tokens[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(MnStaticFieldMiss::NameUnreadable.token(), "name-unreadable");
        assert_eq!(MnStaticFieldMiss::MirrorUnmapped.token(), "mirror-unmapped");
    }
}

/// `MethodHandleNatives.staticFieldOffset(MemberName self)`: the offset
/// `Unsafe.staticFieldOffset` answers for the same field, registered for this
/// VM's Unsafe accesses, so `UNSAFE.getReference(staticFieldBase(mn),
/// staticFieldOffset(mn))` reads it. It answered the member's vmindex, which
/// no Unsafe access resolves, and `staticFieldBase` answered `null`: the
/// JDK's `ClassSpecializer.Factory.readSpeciesDataFromCode` (every
/// `BoundMethodHandle` species, `SimpleMethodHandle` first) failed
/// `BoundMethodHandle.<clinit>` with `InternalError: Failed to link
/// speciesData to speciesCode` (interpreter round i1 wave 50, lane L4).
pub(crate) fn native_mhn_static_field_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let member_name = crate::obj_arg(args, 0)?;
    // Wave 73 (`CRATONVM_MHN_RESOLVE_MEMO`).
    if mhn_resolve_memo_enabled() {
        if let Some(offset) = mhn_static_field_offset_memo(ctx, member_name) {
            return Ok(Some(Value::Long(offset)));
        }
    }
    let Some((class_id, name)) = mn_static_field(ctx, member_name) else {
        return native_mhn_object_field_offset(ctx, args);
    };
    let Some(field_index) = ctx.static_field_index_by_name(class_id, &name) else {
        return native_mhn_object_field_offset(ctx, args);
    };
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let offset = crate::register_static_field_offset_in_vm(
        ctx.vm_identity(),
        &class_name,
        &name,
        class_id,
        field_index,
    );
    // JIT round 20 wave 11: the scaled static byte offset under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (the minted one otherwise).
    let offset = crate::unsafe_natives_ext::unsafe_static_offset_answer(
        &*ctx,
        offset as usize,
        class_id,
        field_index,
    ) as i64;
    Ok(Some(Value::Long(offset)))
}

/// `MethodHandleNatives.staticFieldBase(MemberName self)`: the declaring
/// class's mirror, as `Unsafe.staticFieldBase` answers (see
/// [`native_mhn_static_field_offset`]); `null` for a member that is not a
/// resolved static field.
pub(crate) fn native_mhn_static_field_base(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let member_name = crate::obj_arg(args, 0)?;
    // Wave 73 (`CRATONVM_MHN_RESOLVE_MEMO`).
    if mhn_resolve_memo_enabled() {
        if let Some(class_id) = mhn_static_field_base_memo(ctx, member_name) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(class_id)))));
        }
    }
    match mn_static_field(ctx, member_name) {
        Some((class_id, _)) => Ok(Some(Value::Object(Some(ctx.get_class_mirror(class_id))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// The `(declaring class, class name, field name)` a static-field `MemberName`
/// names, from its real `clazz` and `name` slots.
pub(crate) fn mhn_static_member(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<(ClassId, String, String)> {
    let member_name = crate::obj_arg(args, 0).ok()?;
    let mirror = match mn_get(ctx, member_name, "clazz", MN_CLAZZ) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let field_name = match mn_get(ctx, member_name, "name", MN_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let class_id = ctx.class_id_from_mirror(mirror)?;
    let class_name = ctx.class_name_of_id(class_id)?;
    Some((class_id, class_name, field_name))
}

/// `MethodHandleNatives.setCallSiteTargetNormal(CallSite, MethodHandle)` and
/// (`volatile_store`) `setCallSiteTargetVolatile` (JIT round 20, lane
/// dispatch; registered in `lib.rs` beside the other `MethodHandleNatives`
/// rows). The JDK's `CallSite.setTargetNormal` / `setTargetVolatile` call them
/// after `checkTargetChange`, so the type check is already done: store
/// `target` (the slot [`cs_target_slot`] resolves, the field the call-site
/// natives and the indy bridge read). The volatile form fences on both sides
/// of the store, as a `putReferenceVolatile` would. A null call site is the
/// JVM's `NullPointerException`.
pub(crate) fn native_mhn_set_call_site_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    volatile_store: bool,
) -> MethodCallResult {
    let Some(Value::Object(Some(site))) = args.first().copied() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    if volatile_store {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    cs_write_target(ctx, site, target);
    if volatile_store {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    Ok(None)
}

/// `MethodHandleNatives.getMemberVMInfo(MemberName self)`
///
/// Returns a 2-element Object[] with [vmindex, vmtarget].
pub(crate) fn native_mhn_get_member_vm_info(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let member_name = crate::obj_arg(args, 0)?;
    let vmindex = mn_get_vmindex(ctx, member_name);

    // The shape is dictated by the ONE caller in the whole JDK —
    // `MemberName.vminfoIsConsistent`, which runs only under `assert`:
    //
    //     long vmindex = (Long) ((Object[])vminfo)[0];
    //     Object vmtarget = ((Object[])vminfo)[1];
    //     if (refKindIsField(refKind)) { assert(vmindex >= 0);
    //                                    assert(vmtarget instanceof Class); }
    //     else { assert(refKindDoesDispatch(refKind) ? vmindex >= 0
    //                                                : vmindex < 0);
    //            assert(vmtarget instanceof MemberName); }
    //
    // Because that is the only reader and it was unreachable while `-ea` was
    // being discarded by the launcher, this native's answer was never checked
    // by anything: it boxed an `Integer` where the cast demands a `Long`, and
    // returned the MemberName as `vmtarget` for field kinds too. Both surfaced
    // the moment `-ea` started working — a `ClassCastException: java.lang.Integer
    // cannot be cast to java.lang.Long` out of `MemberName$Factory.resolve`,
    // which killed the VM during `java.lang.invoke` boot.
    let flags = match mn_get(ctx, member_name, "flags", MN_FLAGS) {
        Value::Int(f) => f,
        _ => 0,
    };
    // `MethodHandleNatives.Constants.MN_REFERENCE_KIND_SHIFT` / `_MASK`.
    let ref_kind = (flags >> 24) & 0x0F;
    // REF_getField(1) .. REF_putStatic(4) are the field kinds; REF_invokeVirtual(5)
    // and REF_invokeInterface(9) are the two that dispatch.
    let is_field = (1..=4).contains(&ref_kind);
    let does_dispatch = ref_kind == 5 || ref_kind == 9;

    // CratonVM resolves by name+descriptor and keeps no vtable/itable, so it has
    // no index of HotSpot's kind to report. What it CAN report truthfully is the
    // sign the encoding gives meaning to: "has a dispatch slot" (non-negative)
    // versus "resolved to a single target" (negative). Reporting a non-negative
    // index for an `invokestatic`-kind member would be the actively wrong
    // answer; -1 is the same "no dispatch slot" HotSpot writes there.
    let reported_index: i64 = if is_field || does_dispatch {
        i64::from(vmindex.max(0))
    } else {
        -1
    };

    // For a field the JDK wants the DECLARING CLASS as `vmtarget`, not the
    // MemberName — `clazz` is exactly that, and it is already a mirror.
    let field_target = if is_field {
        match mn_get(ctx, member_name, "clazz", MN_CLAZZ) {
            Value::Object(Some(c)) => Some(c),
            _ => None,
        }
    } else {
        None
    };

    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
    // GC-safety: `box_value` below can trigger a collection that relocates
    // `arr`/`member_name`/`field_target` (all captured or produced above and
    // read again afterward); pin them and re-read the forwarded references
    // before use. `boxed` is the last allocation, so nothing can move it.
    let arr_pin = ctx.pin_native_root(arr);
    let member_name_pin = ctx.pin_native_root(member_name);
    let field_target_pin = field_target.map(|t| (ctx.pin_native_root(t), t));
    // WIDTH is `J`, not `I`: `MemberName$Factory.resolve`'s assertion casts
    // slot 0 to `Long`, which is the bug the comment block above records. That
    // half of this line is settled and must not be reverted to `Value::Int`.
    //
    // ALLOCATOR is `box_value`, i.e. FRESH, and NOT the cached sibling every
    // other boxing site in this file was switched to. That half is a separate,
    // separately-checked decision and it survives the width fix unchanged:
    //
    // This is the one site here whose HotSpot counterpart is not a `valueOf`
    // adapter. `MethodHandleNatives.getMemberVMInfo` is a VM native that fills
    // an `Object[]` with `java_lang_boxing_object::create`-shaped values, the
    // same allocator `Reflection::array_get` uses — and `Array.get` is
    // measured FRESH on both VMs (`array.int` = false, `array.selfid` = false).
    // The slot is JDK-internal plumbing that no Java code identity-compares,
    // so there is no observable to conform to and no reason to put a
    // per-`reported_index` entry into a process-global cache. (The `Long`
    // cache is also the narrower of the two — `-128..=127` — so a `vmindex`
    // outside that window would not be shared anyway.)
    //
    // If a later lane "finishes the job" by switching this to the cached
    // sibling, the thing it will have changed is which of two
    // indistinguishable objects a JDK internal receives — and the thing it
    // will have lost is the annotation saying the difference was checked.
    // Leave it.
    let boxed = crate::lang_class::box_value(ctx, Value::Long(reported_index), "J");
    let arr = ctx.read_native_pin(arr_pin, arr);
    let member_name = ctx.read_native_pin(member_name_pin, member_name);
    let field_target = field_target_pin.map(|(pin, t)| ctx.read_native_pin(pin, t));
    ctx.unpin_native_roots(arr_pin);
    ctx.set_array_element(arr, 0, boxed);
    ctx.set_array_element(
        arr,
        1,
        Value::Object(Some(field_target.unwrap_or(member_name))),
    );
    Ok(Some(Value::Object(Some(arr))))
}

/// `MemberName.type` (slot 2) for the `getMethodType` / `getMethodOrFieldType`
/// natives, as a `MethodType` when the member is a method or constructor
/// whose `type` the JDK left in one of its unconverted shapes: the
/// `Object[] {rtype, ptypes}` the `MemberName(Method)` constructor stores
/// (`this.type = new Object[] { m.getReturnType(), m.getParameterTypes() }`)
/// or a descriptor `String`. Those natives returned the slot raw, so under
/// `--compatible` (where they stand in front of the JDK body) the `Object[]`
/// reached `DirectMethodHandle.make` as a "`MethodType`" with no `ptypes`
/// (`NullPointerException: ... "this.ptypes" is null` in
/// `insertParameterTypes`). The conversion is the JDK's own body
/// (`getMethodType`, which also stores the `MethodType` back), run only for
/// those two shapes; anything else -- a `MethodType`, `null` (whose body
/// would call `expandFromVM`), a field's `Class` -- is the slot as before.
/// Interpreter round i2 wave 56, lane L4
/// (`interpreter-L4-compatible-membername-of-a-method-has-no-parameter-types-FIXED-20261020`).
pub(crate) fn mn_type_slot_converted(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    const IS_METHOD_OR_CONSTRUCTOR: i32 = 0x1_0000 | 0x2_0000;
    let ty = ctx.get_field(this, 2);
    let Value::Object(Some(raw)) = ty else {
        return ty;
    };
    let unconverted = ctx.object_is_array(raw)
        || ctx
            .class_name_arc_of_id(ctx.class_id_of_object(raw))
            .as_deref()
            == Some("java/lang/String");
    let invocable =
        matches!(ctx.get_field(this, 3), Value::Int(f) if f & IS_METHOD_OR_CONSTRUCTOR != 0);
    if !unconverted || !invocable {
        return ty;
    }
    // A Java call (a GC point): `this` is pinned for the fallback read.
    let pin = ctx.pin_native_root(this);
    let converted = ctx.invoke_special_bytecode_only(
        "java/lang/invoke/MemberName",
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(this))],
    );
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    match converted {
        Ok(Some(mt @ Value::Object(Some(_)))) => mt,
        _ => ctx.get_field(this, 2),
    }
}

// ---------------------------------------------------------------------------
// C33 — InvokerBytecodeGenerator bypass
// ---------------------------------------------------------------------------
//
// JDK 25's `java.lang.invoke.InvokerBytecodeGenerator` (called via
// `LambdaForm.compileToBytecode` and, crucially, via `LambdaForm.prepare`)
// drives JEP 466 `java.lang.classfile.*` code-generation to produce invoker
// method classes on the fly. `LambdaForm.compileToBytecode` catches
// `BytecodeGenerationException`, but `LambdaForm.prepare` calls
// `generateLambdaFormInterpreterEntryPoint` WITHOUT a catch — so a BGE from
// the classfile pipeline propagates up through the first MH-using clinit in
// any loaded class (e.g. `org/junit/runner/Result` via its reflection /
// serialization callers).
//
// Our VM does NOT actually dispatch via the generated classes — MH invocation
// is handled by `native_method_handle_link_to` / `mh_dispatch`, which read
// our own MH_* slot layout. So the invoker classes are useless to us. Skip
// the generator entirely by returning a populated MemberName that `prepare`
// can cache as `vmentry` without ever dereferencing.

/// C33: Allocate a minimal, resolved MemberName for code-gen bypass paths.
/// The returned MemberName has:
///   - clazz:      `java/lang/invoke/LambdaForm` mirror (stable, always loaded)
///   - name:       the supplied entry-point name
///   - type:       MethodType built from `desc`, or a `()V` fallback
///   - flags:      IS_METHOD | REF_INVOKE_STATIC<<24 (static invoker)
///   - method:     Int(1) at slot 4 — non-zero sentinel so downstream vmindex
///                 reads (`getMemberVMInfo`, `objectFieldOffset`) don't see 0
///   - resolution: Value::Object(None) at slot 5 — null means `isResolved()`
pub(crate) fn alloc_resolved_member_name(
    ctx: &mut dyn NativeContext,
    host_class: &str,
    name: &str,
    desc: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let mn = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MemberName", MN_FIELD_COUNT)?;

    // `mn` is rooted in a handle scope and re-read after every call that can
    // allocate (the mirror, the name string, and the `MethodType`, which runs
    // Java). It used to be a raw local across all three: a G1 pause in between
    // moved the new `MemberName`, and every later store went through the old
    // address. Two shapes followed (`ObjectStreamLookupGcRaceProbe`, G1, JIT on
    // or off). While the old copy still decoded, `mn_has_synthetic_layout` read
    // a foreign class there and the vmindex sentinel `Int(1)` landed in the real
    // object's `method` slot (the W7-84 guard on `MemberName` index 4). Once the
    // address was re-served, `type`, `flags` and the null `resolution` were
    // written into whatever object lived there: nulled stream-pipeline fields
    // (the early `java.util.stream` NPE), an object reference in an `int` slot,
    // and a zeroed header under live pointer fields (a torn object).
    // `CRATONVM_MN_ALLOC_ROOTED=0` restores the raw local, for an A/B.
    let rooted = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MN_ALLOC_ROOTED");
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let mn_h = scope.root(mn);
    let current = |scope: &cratonvm_native_api::NativeHandleScope<'_>| {
        if rooted {
            scope.get(&mn_h)
        } else {
            mn
        }
    };

    // clazz: use the host class mirror (must be a valid Class mirror for
    // downstream `mn.getDeclaringClass()` reads).
    let host_cid = scope.class_id_by_name(host_class);
    let clazz_mirror = match host_cid.map(|cid| scope.get_class_mirror(cid)) {
        Some(m) => m,
        // Fallback: allocate a synthetic Class stub — should not normally
        // happen since LambdaForm is always loaded before this path.
        None => try_alloc_concurrent_synthetic(&mut *scope, "java/lang/Class", 1)?,
    };
    let mn = current(&scope);
    mn_set(
        &mut *scope,
        mn,
        "clazz",
        MN_CLAZZ,
        Value::Object(Some(clazz_mirror)),
    );

    // name: Java String
    let name_str = scope.create_string(name);
    let mn = current(&scope);
    mn_set(
        &mut *scope,
        mn,
        "name",
        MN_NAME,
        Value::Object(Some(name_str)),
    );

    // type: MethodType built from desc (fall back to ()V if desc is garbage)
    let mt = match build_method_type_from_descriptor(&mut *scope, desc) {
        Ok(Some(mt)) => Ok(Some(mt)),
        Ok(None) => build_method_type_from_descriptor(&mut *scope, "()V"),
        Err(e) => Err(e),
    };
    if let Ok(Some(mt)) = mt {
        let mn = current(&scope);
        mn_set(&mut *scope, mn, "type", MN_TYPE, Value::Object(Some(mt)));
    }

    // flags: IS_METHOD (0x10000) | ACC_STATIC (0x0008) | REF_invokeStatic (6) << 24
    let flags: i32 = 0x1_0000 | 0x0008 | (6 << 24);
    let mn = current(&scope);
    mn_set(&mut *scope, mn, "flags", MN_FLAGS, Value::Int(flags));

    // vmindex: the non-zero "resolved" sentinel, on our fabricated layout only.
    // See `mn_set_vmindex` — index 4 on a real `MemberName` is `method`.
    mn_set_vmindex(&mut *scope, mn, 1);

    // `resolution == null` is what the real `MemberName.isResolved()` reads.
    // Re-read: on the fabricated layout the sentinel above is boxed (allocates).
    let mn = current(&scope);
    mn_set(
        &mut *scope,
        mn,
        "resolution",
        MN_RESOLUTION,
        Value::Object(None),
    );

    Ok(current(&scope))
}

/// C33: Native bypass for
/// `java.lang.invoke.InvokerBytecodeGenerator.generateLambdaFormInterpreterEntryPoint`.
///
/// Called from `LambdaForm.prepare()` in JDK 25. Returns a resolved
/// MemberName for an interpreter entry point. In our VM, actual dispatch
/// never reads `vmentry` — we intercept MH invocation in the interpreter —
/// so any non-null resolved MemberName is sufficient to keep JDK internals
/// happy without triggering JEP 466 code-gen (which in turn fails with
/// `ConstantPoolException: Bad CP index: 0` in our classfile-API stubs).
pub(crate) fn native_ibg_generate_interpreter_entry_point(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = MethodType mt
    let desc = match args.get(0) {
        Some(Value::Object(Some(mt))) => descriptor_from_method_type(ctx, *mt),
        _ => DESC_DEFAULT_METHOD.to_string(),
    };
    let ret_char = desc_close_paren(&desc)
        .and_then(|close| desc[close + 1..].chars().next())
        .unwrap_or('V');
    let name = format!("interpret_{}", ret_char);
    // Interpreter round i2 wave 57, lane L4: the measurement line of
    // `i56-L4-proposal-retire-the-c33-generator-natives` (this one still
    // fabricates with `CRATONVM_IBG_SPIN_ON_MISS` on).
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[IBG_SPIN] generateLambdaFormInterpreterEntryPoint fabricated LambdaForm.{name}{desc}"
        );
    }
    let mn = alloc_resolved_member_name(ctx, "java/lang/invoke/LambdaForm", &name, &desc);
    Ok(Some(Value::Object(Some(mn?))))
}

/// C33: Native bypass for
/// `java.lang.invoke.InvokerBytecodeGenerator.generateCustomizedCode`.
///
/// Called from `LambdaForm.compileToBytecode()` in JDK 25. The caller wraps
/// this in a try/catch(BytecodeGenerationException), but rather than throw
/// from native, we return a resolved MemberName — lets `isCompiled` flip to
/// true without any generator side effects.
pub(crate) fn native_ibg_generate_customized_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: LambdaForm form (0), MethodType invokerType (1)
    //
    // Interpreter round i2 wave 55, lane L4 (`CRATONVM_MH_FOREIGN_VMENTRY_FIRST`):
    // the JDK body's FIRST step, `lookupPregenerated(form, invokerType)` --
    // the form's linker already compiled into `DirectMethodHandle$Holder`,
    // `Invokers$Holder`, `DelegatingMethodHandle$Holder` or
    // `LambdaForm$Holder` -- answers real bytecode, which a real handle's
    // `vmentry` route then runs (`mh_run_foreign_vmentry`). Only a form with no
    // pregenerated linker keeps the fabricated `LambdaForm.MH` (no class is
    // spun). Any failure of the lookup (a species this VM's `speciesDataFor`
    // cannot answer) keeps the fabrication, as before.
    let mut invoker_type = match args.get(1) {
        Some(Value::Object(Some(mt))) => Some(*mt),
        _ => None,
    };
    // Interpreter round i2 wave 56, lane L4 (`CRATONVM_IBG_SPIN_ON_MISS`): the
    // JDK's own body -- `lookupPregenerated`, and on a miss
    // `new InvokerBytecodeGenerator("MH", form, invokerType).loadMethod(..)`,
    // a spun `LambdaForm$MH` class as on HotSpot -- so a real handle over a
    // form with no pregenerated linker has a `vmentry` to run. A failure of
    // the generator keeps the fabrication below.
    if crate::nbflags().ibg_spin_on_miss {
        // The generator runs Java (a GC point): `mt` is read back for the
        // fabrication below.
        let mt_pin = invoker_type.map(|mt| (ctx.pin_native_root(mt), mt));
        let spun = ibg_run_jdk_body(
            ctx,
            "generateCustomizedCode",
            "(Ljava/lang/invoke/LambdaForm;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;",
            args,
        );
        if let Some((pin, mt)) = mt_pin {
            invoker_type = Some(ctx.read_native_pin(pin, mt));
            ctx.unpin_native_roots(pin);
        }
        if let Some(spun) = spun {
            return Ok(Some(Value::Object(Some(spun))));
        }
    }
    if crate::nbflags().mh_foreign_vmentry_first && !crate::nbflags().ibg_spin_on_miss {
        if let (Some(Value::Object(Some(form))), Some(mt)) = (args.first().copied(), invoker_type) {
            // The lookup resolves (a GC point): `mt` is read back for the
            // fabrication below.
            let mt_pin = ctx.pin_native_root(mt);
            let pregenerated = ctx.invoke(
                "java/lang/invoke/InvokerBytecodeGenerator",
                "lookupPregenerated",
                "(Ljava/lang/invoke/LambdaForm;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;",
                &[Value::Object(Some(form)), Value::Object(Some(mt))],
            );
            invoker_type = Some(ctx.read_native_pin(mt_pin, mt));
            ctx.unpin_native_roots(mt_pin);
            if let Ok(Some(Value::Object(Some(pregenerated)))) = pregenerated {
                if crate::nbflags().dbg_mh_dispatch {
                    eprintln!("[MH_FOREIGN] generateCustomizedCode: a pregenerated linker");
                }
                return Ok(Some(Value::Object(Some(pregenerated))));
            }
        }
    }
    let desc = match invoker_type {
        Some(mt) => descriptor_from_method_type(ctx, mt),
        None => DESC_DEFAULT_METHOD.to_string(),
    };
    let mn = alloc_resolved_member_name(ctx, "java/lang/invoke/LambdaForm", "MH", &desc);
    Ok(Some(Value::Object(Some(mn?))))
}

/// C33: Native bypass for
/// `java.lang.invoke.InvokerBytecodeGenerator.generateNamedFunctionInvoker`.
///
/// Called from `NamedFunction.resolve()` in JDK 25 to get a vmentry for a
/// LambdaForm name. Same bypass reasoning as the other two.
pub(crate) fn native_ibg_generate_named_function_invoker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: MethodTypeForm typeForm (0).
    //
    // Interpreter round i2 wave 56, lane L4 (`CRATONVM_IBG_SPIN_ON_MISS`): the
    // JDK's own body (a spun `invoke_<sig>` of `INVOKER_METHOD_TYPE`). The
    // fabrication below is typed `()V`, which `NamedFunction.computeInvoker`
    // refuses with an `InternalError` on every use, so `interpretWithArguments`
    // could not run one `NamedFunction` here.
    if crate::nbflags().ibg_spin_on_miss {
        if let Some(spun) = ibg_run_jdk_body(
            ctx,
            "generateNamedFunctionInvoker",
            "(Ljava/lang/invoke/MethodTypeForm;)Ljava/lang/invoke/MemberName;",
            args,
        ) {
            return Ok(Some(Value::Object(Some(spun))));
        }
    }
    let mn = alloc_resolved_member_name(ctx, "java/lang/invoke/LambdaForm", "NFI", "()V");
    Ok(Some(Value::Object(Some(mn?))))
}

/// Run the JDK's own bytecode body of the held
/// `InvokerBytecodeGenerator.<method>` (`CRATONVM_IBG_SPIN_ON_MISS`): its
/// `MemberName`, or `None` when it fails (an exception from the class-file
/// generator or the hidden-class definition), whose caller keeps the C33
/// fabrication. `invoke_special_bytecode_only` skips this native's own
/// registration, so the body does not re-enter it (a NESTED form compiled
/// while the generator runs does, which is HotSpot's order too). Interpreter
/// round i2 wave 56, lane L4.
#[cold]
#[inline(never)]
pub(crate) fn ibg_run_jdk_body(
    ctx: &mut dyn NativeContext,
    method: &str,
    desc: &str,
    args: &[Value],
) -> Option<ObjectRef> {
    let answer = ctx.invoke_special_bytecode_only(
        "java/lang/invoke/InvokerBytecodeGenerator",
        method,
        desc,
        args,
    );
    match answer {
        Ok(Some(Value::Object(Some(mn)))) => {
            if crate::nbflags().dbg_mh_dispatch {
                let name = match ctx.get_field_by_name(mn, "name") {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                let class = match ctx.get_field_by_name(mn, "clazz") {
                    Value::Object(Some(m)) => resolve_class_name_robust(ctx, m).unwrap_or_default(),
                    _ => String::new(),
                };
                eprintln!("[IBG_SPIN] {method}: {class}.{name}");
            }
            Some(mn)
        }
        other => {
            if crate::nbflags().dbg_mh_dispatch {
                let why = match &other {
                    Err(e) => format!("{e:?}"),
                    Ok(v) => format!("{v:?}"),
                };
                let why: String = why.chars().take(300).collect();
                eprintln!("[IBG_SPIN] {method} failed, fabricating: {why}");
            }
            None
        }
    }
}

/// `MethodHandles.byteArrayViewVarHandle` bounds.
///
/// Measured against HotSpot JDK 25 with the same three-line probe: a `short`
/// view over a 16-byte array reports `Index -1 out of bounds for length 15` —
/// `15`, not `16`, because the length in the message is the number of valid
/// START positions. CratonVM checked nothing at all, so `set(m, -1, v)` was an
/// out-of-bounds write that reported success.
#[cfg(test)]
mod byte_array_view_bounds_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeHeapAccess;

    /// `(width, valid start positions)` for a 16-byte array.
    const CASES: &[(u8, i32)] = &[
        (b'S', 15),
        (b'C', 15),
        (b'I', 13),
        (b'F', 13),
        (b'J', 9),
        (b'D', 9),
    ];

    #[test]
    fn a_negative_index_is_refused_with_the_jdk_message() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        for (elem, limit) in CASES {
            let err = byte_view_check_index(&ctx, arr, -1, *elem)
                .expect_err("a negative index must be refused");
            let text = format!("{err:?}");
            assert!(
                text.contains(&format!("Index -1 out of bounds for length {limit}")),
                "elem {}: {text}",
                *elem as char
            );
        }
    }

    #[test]
    fn the_last_valid_start_is_accepted_and_the_next_one_is_not() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        for (elem, limit) in CASES {
            assert!(
                byte_view_check_index(&ctx, arr, limit - 1, *elem).is_ok(),
                "elem {}: index {} must fit",
                *elem as char,
                limit - 1
            );
            assert!(
                byte_view_check_index(&ctx, arr, *limit, *elem).is_err(),
                "elem {}: index {} must not fit",
                *elem as char,
                limit
            );
        }
    }

    #[test]
    fn an_array_too_short_for_one_element_admits_no_index() {
        let mut ctx = mock_ctx();
        // Four bytes cannot hold a long; HotSpot reports the signed -3 bound.
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        let err = byte_view_check_index(&ctx, arr, 0, b'J').expect_err("no index can fit");
        assert!(
            format!("{err:?}").contains("Index 0 out of bounds for length -3"),
            "{err:?}"
        );
    }
}



/// `MethodHandles.insertArguments(target, pos, values[])` over this VM's handle model
/// (`MH_KIND_INSERT`). Also the body of the `MethodHandle.bindArgument*` natives below.
pub(crate) fn native_insert_arguments(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // STATIC method: args[0]=target MH, args[1]=int pos, args[2]=values[].
    //
    // Round 15 wave 6 (lane mh5; `r15w5-mh4-review-findings` item 2):
    // the JDK body's first two reads are `values.length` and then
    // `target.type()` (`insertArgumentsChecks`), so a null `values`
    // is that NPE before anything else, and a null `target` the next
    // one. This bridge minted an adapter for the first (typed by the
    // leaf's `MH_DESC`) and answered `null` for the second. java.base
    // is compiled with its LocalVariableTable (the measured helpful
    // NPEs this file transcribes name `newType`, `targetClass`), so
    // the messages name the parameters.
    if let Some(refusal) = insert_arguments_null_refusal(args) {
        if mh_insert_arguments_null_npe_enabled() {
            return Err(refusal);
        }
    }
    let target = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    };
    let pos = match args.get(1) {
        Some(Value::Int(p)) => *p,
        _ => 0,
    };
    let values = args.get(2).copied().unwrap_or(Value::Object(None));
    // `insertArguments` refuses when `pos` + `values.length` exceeds
    // the target's own parameter count — javadoc's `0 <= pos <= pos +
    // values.length <= target.type().parameterCount()`. Measured
    // (`p.insertTooMany`, `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md`): three values inserted at position 1 into a
    // 3-parameter target were silently accepted where HotSpot 25
    // throws `IllegalArgumentException` at this call. Every way of
    // not resolving the target's descriptor is an ACCEPT — this only
    // refuses on a positive over-supply reading.
    //
    // Wave 23 (lane L4): HotSpot's two checks and messages
    // (`MethodHandleImpl.insertArgumentsChecks`: "too many values to
    // insert" when the values outnumber the parameters, else "no
    // argument type to append" for a `pos` outside `0..=params -
    // values`; the single message this used, with its numbers, was
    // CratonVM's own), then each value against its parameter as the
    // JDK binds it (`Wrapper.convert` for a primitive, `Class.cast`
    // for a reference; `mh_bind_value_refusal`), in order.
    if let (Value::Object(Some(arr)), Some(tdesc)) = (
        values,
        mh_type_descriptor(ctx, target).or_else(|| mh_read_desc(ctx, target)),
    ) {
        if let Some((params, _)) = split_descriptor_params(&tdesc) {
            let nvalues = ctx.array_length(arr);
            if nvalues > params.len() {
                return Err(mh_illegal_argument("too many values to insert".to_string()));
            }
            let inargs = params.len() - nvalues;
            if pos < 0 || pos as usize > inargs {
                return Err(mh_illegal_argument(
                    "no argument type to append".to_string(),
                ));
            }
            for i in 0..nvalues {
                let value = ctx.get_array_element(arr, i);
                if let Some(refusal) = mh_bind_value_refusal(ctx, &params[pos as usize + i], value)
                {
                    return Err(refusal);
                }
            }
        }
    }
    // Round 15 wave 5 (lane mh4): no values is `target` itself
    // (`if (insCount == 0) return target;`, after the checks). The
    // adapter minted here for it carried the LEAF's `MH_DESC` as its
    // `type` (the retype below needs a value), one parameter short of
    // a virtual target's, which the adapter arity refusal
    // (`bound_handle_arity_refusal`) would then judge.
    if let Value::Object(Some(arr)) = values {
        if ctx.array_length(arr) == 0 && mh_invoke_adapter_arity_enabled() {
            return Ok(Some(Value::Object(Some(target))));
        }
    }
    // gc-common w20-f: the descriptor is read and `target` / `values`
    // pinned BEFORE the carrier's allocation. They were stored into
    // the carrier and pinned after it, i.e. at their pre-allocation
    // addresses; `target_pin` (lowest) now releases all at the end.
    let desc = mh_carrier_leaf_desc(ctx, target);
    let target_pin = ctx.pin_native_root(target);
    let values_pin = pin_mh_value(ctx, values);
    let wrapper = alloc_mh_carrier_holding(
        ctx,
        "__mh_insert_wrapper__",
        &[Value::Object(Some(target)), values, Value::Int(pos)],
    );
    // gen r4w3/rooting: `alloc_method_handle` and
    // `build_method_type_from_descriptor` are GC points; `wrapper`,
    // `target`, `values` and `adapter` are all used after them. Pin
    // and re-read.
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let adapter = alloc_method_handle(ctx, "__adapter__", "insert", &desc, MH_KIND_INSERT)?;
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    let target = ctx.read_native_pin(target_pin, target);
    let values = read_pinned_mh_arg(ctx, values_pin, values);
    let adapter_pin = ctx.pin_native_root(adapter);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    // type(): insertArguments at `pos` REMOVES `values.length` parameters
    // (the bound ones) from the target's type. Chain off the target's
    // adapted type so stacked combinators stay arity-correct. Without
    // this the guard MH that Groovy's IndyInterface builds
    // (`insertArguments(sameClasses,0,classes).asCollector(...)`) had a
    // classes[] longer than the collected args -> AIOOBE in sameClasses.
    let nvalues = match values {
        Value::Object(Some(arr)) => ctx.array_length(arr),
        _ => 0,
    };
    if nvalues > 0 {
        if let Some(tdesc) = mh_type_descriptor(ctx, target) {
            if let Some((mut params, ret)) = split_descriptor_params(&tdesc) {
                let p = (pos.max(0) as usize).min(params.len());
                let end = (p + nvalues).min(params.len());
                if p < end {
                    params.drain(p..end);
                }
                let new_desc = format!("({}){}", params.concat(), ret);
                if let Ok(Some(mt)) = build_method_type_from_descriptor(ctx, &new_desc) {
                    let adapter = ctx.read_native_pin(adapter_pin, adapter);
                    ctx.set_field_by_name(adapter, "type", Value::Object(Some(mt)));
                }
            }
        }
    }
    let adapter = ctx.read_native_pin(adapter_pin, adapter);
    ctx.unpin_native_roots(target_pin);
    Ok(Some(Value::Object(Some(adapter))))
}

