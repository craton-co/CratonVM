#[allow(unused_imports)]
use super::*;

/// What `ObjectMethods.makeToString` throws for a getter whose handle type is
/// not `(R)T`: an `IllegalArgumentException(message)`, wrapped in a
/// `RuntimeException` when it comes from inside `makeToString`'s `try`
/// (`filterArguments` / `permuteArguments`), bare when it comes from
/// `MethodType.methodType` before it (a `void` return).
pub(in crate::runtime::invokedynamic) struct ObjectMethodsGetterRefusal {
    pub(in crate::runtime::invokedynamic) message: String,
    pub(in crate::runtime::invokedynamic) wrapped: bool,
}

/// `--real-jdk`, a `toString` site: `Some` when `makeToString` refuses the
/// getters' handle types, decided from the pool and the loaded classes, in
/// the JDK's order (read from `java/lang/runtime/ObjectMethods.java`,
/// `MethodHandles.filterArguments` / `permuteArgumentChecks`, JDK 25; the
/// messages measured on HotSpot 25, probe
/// `L4W42ObjectMethodsStaticSpecialGetters.java`):
///
/// 1. a `void` return anywhere: `MethodType.methodType(String, returns)` —
///    `parameter type cannot be void`, not wrapped;
/// 2. `filterArguments` checks the getters LAST FIRST against the concat
///    type `(T0..Tn)String`: a getter that does not take exactly one
///    argument — `target and filter types do not match: <concat>, <getter>`;
/// 3. `permuteArguments` to `(R)String`: a getter whose one parameter is not
///    `R` — `parameter types do not match after reorder: <filtered>,
///    (R)String`, where `<filtered>` has each getter's parameter.
///
/// `None` when every getter is `(R)T`, and when a type the message needs
/// cannot be decided here: a receiver type HotSpot may restrict to the
/// caller (a protected member of another class, a special getter of another
/// class), or more than 200 argument slots (`makeToString` then splits the
/// getters, and the messages name each split). Such a site keeps the JDK's
/// route. javac's shape (every getter a field of the record) is never
/// refused and pays one scan of the pool.
///
/// Interpreter round i1 wave 42, lane L4
/// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`).
pub(in crate::runtime::invokedynamic) fn object_methods_to_string_getter_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    info: &IndyInfo,
) -> Option<ObjectMethodsGetterRefusal> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    let cp = &class.constant_pool;
    let args = &info.bootstrap_arg_indices;
    if object_methods_getters_are_record_fields(cp, args) {
        return None;
    }
    let record = cp.get_class_name(*args.first()?)?;
    let record_desc = format!("L{record};");
    let caller_desc = format!("L{};", class.name);
    // `true` when the member `h` names is found on its class and is not
    // protected: its handle's receiver type is then that class. A protected
    // member of another package may be restricted to the caller's type.
    let receiver_unrestricted = |h: &MethodHandle, field: bool| -> bool {
        let Some(owner) = cm.find_class_by_name_for_class(&h.class_name, class_id) else {
            return false;
        };
        let flags = if field {
            crate::classloading::find_field_recursive_by_descriptor(
                owner,
                &h.member_name,
                &h.descriptor,
                &cm.class_store,
            )
            .map(|(_, f, _)| f.access_flags.bits())
        } else {
            crate::classloading::find_method_recursive(
                owner,
                &h.member_name,
                &h.descriptor,
                &cm.class_store,
            )
            .map(|(m, _)| m.access_flags.bits())
        };
        matches!(flags, Some(bits) if bits & 0x0004 == 0)
    };
    // Each getter's handle type: its parameters (`None` for a receiver type
    // not decided here) and its return.
    let mut types: Vec<(Vec<Option<String>>, String)> = Vec::with_capacity(args.len());
    for &idx in args.iter().skip(2) {
        let h = resolve_method_handle_full(cp, idx).ok()?;
        let own_class = &*h.class_name == record;
        let owner_desc = if h.class_name.starts_with('[') {
            h.class_name.to_string()
        } else {
            format!("L{};", h.class_name)
        };
        let receiver = |field: bool| -> Option<String> {
            if own_class {
                Some(record_desc.clone())
            } else if !h.class_name.starts_with('[') && receiver_unrestricted(&h, field) {
                Some(owner_desc.clone())
            } else {
                None
            }
        };
        let (params, ret): (Vec<Option<String>>, String) = match h.kind {
            MethodHandleKind::GetField => (vec![receiver(true)], h.descriptor.to_string()),
            MethodHandleKind::GetStatic => (Vec::new(), h.descriptor.to_string()),
            MethodHandleKind::PutField => (
                vec![receiver(true), Some(h.descriptor.to_string())],
                "V".to_string(),
            ),
            MethodHandleKind::PutStatic => (vec![Some(h.descriptor.to_string())], "V".to_string()),
            MethodHandleKind::InvokeStatic
            | MethodHandleKind::InvokeVirtual
            | MethodHandleKind::InvokeInterface
            | MethodHandleKind::InvokeSpecial
            | MethodHandleKind::NewInvokeSpecial => {
                let (ps, r) = split(&h.descriptor);
                let mut params: Vec<Option<String>> = Vec::with_capacity(ps.len() + 1);
                match h.kind {
                    MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                        params.push(receiver(false));
                    }
                    // `findSpecial`'s receiver is the calling class.
                    MethodHandleKind::InvokeSpecial => {
                        params.push((owner_desc == caller_desc).then(|| caller_desc.clone()))
                    }
                    _ => {}
                }
                params.extend(ps.iter().map(|p| Some((*p).to_string())));
                let ret = if h.kind == MethodHandleKind::NewInvokeSpecial {
                    owner_desc.clone()
                } else {
                    r.to_string()
                };
                (params, ret)
            }
        };
        types.push((params, ret));
    }
    let refusal =
        |message: String, wrapped: bool| Some(ObjectMethodsGetterRefusal { message, wrapped });
    if types.iter().any(|(_, ret)| ret == "V") {
        return refusal("parameter type cannot be void".to_string(), false);
    }
    let slots: usize = types
        .iter()
        .map(|(_, ret)| if ret == "J" || ret == "D" { 2 } else { 1 })
        .sum();
    if slots > 200 {
        return None;
    }
    let returns: String = types.iter().map(|(_, ret)| ret.as_str()).collect();
    let concat = format!("({returns})Ljava/lang/String;");
    let display = |descriptor: &str| method_type_display_for(&*cm, class_id, descriptor);
    for (params, ret) in types.iter().rev() {
        if params.len() != 1 {
            let params: Option<Vec<&str>> = params.iter().map(|p| p.as_deref()).collect();
            let filter = format!("({}){ret}", params?.concat());
            return refusal(
                format!(
                    "target and filter types do not match: {}, {}",
                    display(&concat),
                    display(&filter)
                ),
                true,
            );
        }
    }
    if types
        .iter()
        .all(|(params, _)| params[0].as_deref() == Some(record_desc.as_str()))
    {
        return None;
    }
    let firsts: Option<Vec<&str>> = types
        .iter()
        .map(|(params, _)| params[0].as_deref())
        .collect();
    let filtered = format!("({})Ljava/lang/String;", firsts?.concat());
    let wanted = format!("({record_desc})Ljava/lang/String;");
    refusal(
        format!(
            "parameter types do not match after reorder: {}, {}",
            display(&filtered),
            display(&wanted)
        ),
        true,
    )
}

/// The `BootstrapMethodError` of an [`ObjectMethodsGetterRefusal`]:
/// `bootstrap method initialization exception`, caused by the
/// `IllegalArgumentException`, or by a `RuntimeException` (whose message is
/// the wrapped exception's `toString()`, as `new RuntimeException(t)` makes
/// it) caused by it.
pub(in crate::runtime::invokedynamic) fn object_methods_to_string_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    refusal: &ObjectMethodsGetterRefusal,
) -> MethodCallFailed {
    const BME: &str = "bootstrap method initialization exception";
    let iae = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/IllegalArgumentException",
        Some(refusal.message.as_str()),
    ) {
        Ok(iae) => iae,
        Err(e) => return e,
    };
    if !refusal.wrapped {
        return bootstrap_method_error(shared, thread, BME, iae);
    }
    let wrapper_message = format!("java.lang.IllegalArgumentException: {}", refusal.message);
    match throwable_with_cause(
        shared,
        thread,
        "java/lang/RuntimeException",
        &wrapper_message,
        iae,
    ) {
        MethodCallFailed::ExceptionThrown(wrapper) => {
            bootstrap_method_error(shared, thread, BME, wrapper)
        }
        other => other,
    }
}

/// `--real-jdk`: resolve the `MethodHandle` static arguments (the getters) of
/// an `ObjectMethods.bootstrap` site that is not javac's shape as `ldc` of the
/// same entries resolves them (JVMS 5.4.3.5, through `Lookup.find*` with the
/// calling class's access, and through the same per-entry record, so a
/// failure is recorded and rethrown). HotSpot resolves every static argument
/// before the bootstrap runs: a getter of another class's private field is
/// `IllegalAccessError` (cause `IllegalAccessException`), a missing field
/// `NoSuchFieldError`. Interpreter round i1 wave 39, lane L4 (item 1 of
/// `i38-L4-objectmethods-native-linkage-ignores-its-getters`). javac's shape
/// (the record's own fields, from the record) cannot fail and is skipped, so
/// javac output pays nothing.
pub(in crate::runtime::invokedynamic) fn object_methods_resolve_getters(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let (handles, caller_name): (Vec<(u16, MethodHandle)>, String) = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(());
        };
        if class.object_methods_args_are_canonical(&info.bootstrap_arg_indices) {
            return Ok(());
        }
        let handles = info
            .bootstrap_arg_indices
            .iter()
            .skip(2)
            .filter_map(|&idx| {
                resolve_method_handle_full(&class.constant_pool, idx)
                    .ok()
                    .map(|h| (idx, h))
            })
            .collect();
        (handles, class.name.replace('/', "."))
    };
    for (idx, handle) in &handles {
        // A field getter's JVMS 5.4.4 check, made here rather than left to
        // `Lookup.findGetter` (wave 39 follow-up: on the host the lookup
        // route admitted another class's private field). The owner is
        // resolved through the caller's loader first, as the resolution does.
        let field_kind = matches!(
            handle.kind,
            MethodHandleKind::GetField
                | MethodHandleKind::GetStatic
                | MethodHandleKind::PutField
                | MethodHandleKind::PutStatic
        );
        let refused_private = if field_kind && !handle.class_name.starts_with('[') {
            let owner = crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                class_id,
                &handle.class_name,
            )
            .ok();
            // A verdict resting on a `NestHost` claim no door has loaded the
            // host of is asked again once it is loaded (JIT round 20 wave 16,
            // lane nest; `field_access::settle_nest_host_claims`).
            let refusal = |owner: ClassId, nest_unsettled: &std::cell::Cell<bool>| {
                let cm = shared.classes.class_manager.read();
                crate::runtime::interpreter::field_access::field_handle_access_refusal(
                    shared,
                    &cm,
                    class_id,
                    owner,
                    &handle.member_name,
                    &handle.descriptor,
                    nest_unsettled,
                )
            };
            let nest_unsettled = std::cell::Cell::new(false);
            let nest_mark = crate::runtime::interpreter::field_access::nest_host_claim_mark(shared);
            let first = owner.and_then(|owner| refusal(owner, &nest_unsettled));
            let verdict = if nest_unsettled.get() {
                crate::runtime::interpreter::field_access::settle_nest_host_claims(
                    shared, thread, nest_mark,
                )?;
                owner.and_then(|owner| refusal(owner, &std::cell::Cell::new(false)))
            } else {
                first
            };
            verdict
                // PRIVATE only (wave 42): any other refusal is the `ldc` route's
                // below (`method_handle_constant_access_refusal`), which refuses
                // a package-private member of another package with HotSpot's
                // `member is private to package` and admits a protected one (its
                // handle's receiver restricted to the caller). This pre-check
                // refused a protected field too, as `member is not accessible`.
                .filter(|&private| private)
        } else {
            None
        };
        if let Some(private) = refused_private {
            // `MemberName.makeAccessException`'s text (`member is private:
            // R.b/java.lang.String/getField, from class C (...)`, measured
            // up to the module part), as an `IllegalAccessError` caused by
            // the `IllegalAccessException` (`mapLookupExceptionToError`).
            let kind = match handle.kind {
                MethodHandleKind::GetField => "getField",
                MethodHandleKind::GetStatic => "getStatic",
                MethodHandleKind::PutField => "putField",
                _ => "putStatic",
            };
            let message = format!(
                "member is {}: {}.{}/{}/{kind}, from class {caller_name}",
                if private { "private" } else { "not accessible" },
                handle.class_name.replace('/', "."),
                handle.member_name,
                descriptor_class_name(&handle.descriptor),
            );
            if dbg_indy_all() {
                eprintln!(
                    "[indy-all] object-methods {} getter cp#{idx}: access refused: {message}",
                    info.target_name
                );
            }
            let refuse = |t: &mut JvmThread| -> Result<ObjectRef, MethodCallFailed> {
                let cause = crate::runtime::exceptions::create_exception_object(
                    shared,
                    t,
                    "java/lang/IllegalAccessException",
                    Some(message.as_str()),
                )?;
                Err(throwable_with_cause(
                    shared,
                    t,
                    "java/lang/IllegalAccessError",
                    &message,
                    cause,
                ))
            };
            resolve_recorded_static_arg(
                shared,
                thread,
                class_id,
                *idx,
                true,
                info.fill_as_of,
                refuse,
            )?;
            continue;
        }
        let resolve = |t: &mut JvmThread| {
            crate::runtime::interpreter::constants::resolve_method_handle_constant(
                shared,
                t,
                class_id,
                handle.kind,
                &handle.class_name,
                &handle.member_name,
                &handle.descriptor,
            )
        };
        resolve_recorded_static_arg(
            shared,
            thread,
            class_id,
            *idx,
            true,
            info.fill_as_of,
            resolve,
        )?;
    }
    Ok(())
}

/// Does this `StringConcatFactory` site need the JDK's own factory
/// (`bootstrap_generic`) rather than the native linkage? `true` when
/// `makeConcatWithConstants`' checks would refuse it (the recipe's argument
/// tags against the call type, its constant tags against the constants, a
/// return type that cannot hold a `String`, more than 200 argument slots) and
/// when a constant is a kind the native renderer does not model (a
/// `MethodType`, a `MethodHandle`, a dynamic constant). A `Class` constant
/// stays native, rendered as `String.valueOf(Class)` renders it: the JDK
/// factory's own handle chain does not link on CratonVM for it
/// (`AbstractMethodError`, measured). javac emits none of these; each is a
/// hand-assembled site.
pub(in crate::runtime::invokedynamic) fn concat_needs_the_jdk_factory(shared: &SharedVm, class_id: ClassId, info: &IndyInfo) -> bool {
    let params = parse_descriptor_args(&info.target_descriptor);
    let slots: usize = params
        .iter()
        .map(|c| if matches!(c, 'J' | 'D') { 2 } else { 1 })
        .sum();
    if slots > 200 {
        return true;
    }
    let ret = info
        .target_descriptor
        .rsplit_once(')')
        .map(|(_, r)| r)
        .unwrap_or("");
    if !matches!(
        ret,
        "Ljava/lang/String;"
            | "Ljava/lang/Object;"
            | "Ljava/lang/CharSequence;"
            | "Ljava/lang/Comparable;"
            | "Ljava/io/Serializable;"
            | "Ljava/lang/constant/Constable;"
            | "Ljava/lang/constant/ConstantDesc;"
    ) {
        return true;
    }
    if info.bsm_method != MAKE_CONCAT_WITH_CONSTANTS {
        return false;
    }
    let arg_tags = info.recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count();
    let const_tags = info.recipe.iter().filter(|&&u| u == TAG_CONST_UNIT).count();
    if arg_tags != params.len() || const_tags != info.constant_args.len() {
        return true;
    }
    // Constant kinds, read from the pool: everything past the recipe.
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    let recipe_ok = info.bootstrap_arg_indices.first().is_some_and(|&idx| {
        matches!(
            class.constant_pool.get(idx),
            Some(ConstantPoolEntry::StringReference { .. } | ConstantPoolEntry::Utf8(_))
        ) && resolve_concat_constant_units(&class.constant_pool, idx).is_some()
    });
    if !recipe_ok {
        return true;
    }
    info.bootstrap_arg_indices.iter().skip(1).any(|&idx| {
        !matches!(
            class.constant_pool.get(idx),
            Some(
                ConstantPoolEntry::StringReference { .. }
                    | ConstantPoolEntry::ClassReference { .. }
                    | ConstantPoolEntry::Integer(_)
                    | ConstantPoolEntry::Float(_)
                    | ConstantPoolEntry::Long(_)
                    | ConstantPoolEntry::Double(_)
            )
        ) || resolve_concat_constant_units(&class.constant_pool, idx).is_none()
    })
}

/// Generic invokedynamic linkage: execute an arbitrary bootstrap method to
/// obtain a `CallSite`, then invoke its target `MethodHandle`.
///
/// Which linkages are published for later executions is
/// [`GenericIndyCacheMode`]'s choice; an unpublished one bootstraps again on the
/// next execution, as every generic site did before the cache existed.
pub(in crate::runtime::invokedynamic) fn bootstrap_generic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    bootstrap_generic_with(
        shared,
        thread,
        frame_idx,
        cp_index,
        info,
        current_class_id,
        false,
    )
}

/// [`bootstrap_generic`] through the JDK's own bytecode of a static
/// bootstrap: `LambdaMetafactory.metafactory` / `altMetafactory` carry a
/// registered `Bridge` (for REFLECTIVE callers, `lang_invoke.rs`) that builds
/// a proxy without the JDK's checks, so the route
/// [`ClassRealm::indy_other_shapes_to_jdk`] takes for a lambda site that is
/// not javac's shape runs the real `LambdaMetafactory` instead (measured on
/// the wave-48 build: it links and refuses as HotSpot does, spinning its
/// hidden class). Interpreter round i1 wave 48, lane L4.
///
/// [`ClassRealm::indy_other_shapes_to_jdk`]: crate::vm::realms::ClassRealm::indy_other_shapes_to_jdk
pub(in crate::runtime::invokedynamic) fn bootstrap_generic_jdk(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if dbg_indy_all() {
        eprintln!(
            "[indy-all] other shape cp#{cp_index}: jdk bootstrap {}.{}",
            info.bsm_class, info.bsm_method
        );
    }
    bootstrap_generic_with(
        shared,
        thread,
        frame_idx,
        cp_index,
        info,
        current_class_id,
        true,
    )
}

pub(in crate::runtime::invokedynamic) fn bootstrap_generic_with(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
    jdk_bytecode: bool,
) -> Result<(), MethodCallFailed> {
    let mode = generic_indy_cache_mode();
    let shape = Arc::new(GenericIndyShape::new(&info.target_descriptor));
    // Every pin `link_generic_call_site` pushes is released here, on every
    // path: several of its `?`s used to return with their pins still pushed.
    let pin_base = thread.native_pin_roots.len();
    let linked = link_generic_call_site(
        shared,
        thread,
        cp_index,
        info,
        current_class_id,
        jdk_bytecode,
    );
    thread.native_pin_roots.truncate(pin_base);
    let call_site = if mode == GenericIndyCacheMode::Off {
        linked?
    } else {
        settle_generic_link(
            shared,
            thread,
            frame_idx,
            current_class_id,
            cp_index,
            mode.caches_mutable_call_site_of(&info.bsm_class),
            &shape,
            linked,
        )?
    };
    invoke_generic_call_site(shared, thread, frame_idx, call_site, &shape)
}

/// Run the bootstrap method of a generic `invokedynamic` and return the
/// `CallSite` it produced, UNPINNED — the caller pins it before its next
/// safepoint. A failure is already wrapped per JVMS §6.5.
pub(in crate::runtime::invokedynamic) fn link_generic_call_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
    jdk_bytecode: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // --- Re-resolve the BSM (with descriptor) + static args under the lock. ---
    let (bsm_class, bsm_method, bsm_desc, static_args, bsm_is_static, bsm_ref) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic generic: class {current_class_id} not found"),
            })?;
        let bsm_index = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::InvokeDynamic {
                bootstrap_method_attr_index,
                ..
            }) => *bootstrap_method_attr_index,
            _ => {
                return Err(VmError::Internal {
                    message: format!(
                        "invokedynamic generic cp#{cp_index}: not an InvokeDynamic entry"
                    ),
                }
                .into())
            }
        };
        let bsm = class
            .bootstrap_methods
            .get(bsm_index as usize)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic generic: bsm index {bsm_index} out of bounds"),
            })?;
        let h = resolve_method_handle_full(&class.constant_pool, bsm.bootstrap_method_ref)?;
        let sargs: Result<Vec<StaticArg>, MethodCallFailed> = bsm
            .bootstrap_arguments
            .iter()
            .map(|&idx| resolve_static_arg_kind(&class.constant_pool, &class.name, idx))
            .collect();
        (
            h.class_name.to_string(),
            h.member_name.to_string(),
            h.descriptor.to_string(),
            sargs,
            h.kind == MethodHandleKind::InvokeStatic,
            // Wave 119 (`CRATONVM_INDY_GENERIC_VIA_MAKESITE`): the bootstrap's
            // own `CONSTANT_MethodHandle` entry and kind, for its handle.
            (bsm.bootstrap_method_ref, h.kind),
        )
    };
    // A malformed argument is a Java `ClassFormatError`, built only now that
    // the `class_manager` lock is released.
    let static_args = static_args.map_err(|e| match e {
        MethodCallFailed::InternalError(VmError::Linkage(l)) => {
            crate::runtime::exceptions::throw_linkage_error(shared, thread, l)
        }
        other => other,
    })?;

    // --- Build bootstrap args: (Lookup, name, MethodType, static args...). ---
    // Every reference-typed arg is pinned in `native_pin_roots` (a scanned +
    // forwarded GC root) and re-read from its slot before use, because the
    // allocations below — and the bootstrap invocation itself — can move the
    // heap. `bsm_pins` maps an arg position to its pin slot.
    let pin_base = thread.native_pin_roots.len();
    let bsm_class_id = if bsm_is_static && !jdk_bytecode {
        Some(
            crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                &bsm_class,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found(shared, thread, &bsm_class, e)
            })?,
        )
    } else {
        None
    };

    let mut bsm_args: Vec<Value> = Vec::with_capacity(3 + static_args.len());
    let mut bsm_pins: Vec<(usize, usize)> = Vec::new();

    // 1. Lookup for the caller class. `MethodHandles.lookup()` is
    //    caller-sensitive; invoked from here the innermost Java frame is the
    //    current indy method, so `lookupClass` resolves to `current_class_id`.
    let lookup = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )?
    .unwrap_or(Value::Object(None));
    if let Value::Object(Some(o)) = lookup {
        bsm_pins.push((0, thread.native_pin_roots.len()));
        thread.native_pin_roots.push(o);
    }
    bsm_args.push(lookup);

    // 2. The call-site name. Every allocation of this argument list follows
    //    `new`'s contract (collect, retry, `OutOfMemoryError`); the infallible
    //    `create_java_string` / class mirror aborted the process when a link
    //    met a full heap (gc-common w9-c,
    //    `common-w8v-constant-resolution-strings-abort-on-a-full-heap`). A
    //    collection moves objects: every reference is pinned the moment it
    //    exists, and `bsm_args` is rebuilt from the pins after the static
    //    arguments. An early `?` leaves pins behind for the caller
    //    (`link_generic_call_site`'s caller truncates to its own base).
    let name_ref = crate::runtime::interpreter::intern_string_literal_or_oom(
        shared,
        thread,
        &info.target_name,
    )?;
    bsm_pins.push((1, thread.native_pin_roots.len()));
    thread.native_pin_roots.push(name_ref);
    bsm_args.push(Value::Object(Some(name_ref)));

    // 3. The call-site MethodType, resolved as `ldc` of a `CONSTANT_MethodType`
    //    with the same descriptor is (JVMS §5.4.3.6): through the CALLER's
    //    defining loader, and interned (`fromMethodDescriptorString`), as the
    //    `MethodType` static arguments already were. It used to come from the
    //    by-name builder — a fresh, un-interned object whose mirrors ignored
    //    the caller's loader — and a failure was a VM-internal error; now a
    //    failure is the Java error, recorded against this instruction by
    //    `settle_generic_link`. Memoised in the `ldc` record under this
    //    `CONSTANT_InvokeDynamic` index (never an `ldc` operand), because a
    //    `MutableCallSite` bootstrap re-runs per execution in the default
    //    cache mode and the factory runs Java.
    let mt = match crate::runtime::interpreter::constants::probe_recorded_cp_constant(
        shared,
        current_class_id,
        cp_index,
    ) {
        Some(Value::Object(Some(mt))) => mt,
        _ => {
            let mt = crate::runtime::interpreter::constants::resolve_method_type_constant(
                shared,
                thread,
                current_class_id,
                &info.target_descriptor,
            )?;
            crate::runtime::interpreter::constants::store_recorded_cp_constant_as_of(
                shared,
                current_class_id,
                cp_index,
                Value::Object(Some(mt)),
                info.fill_as_of,
            );
            mt
        }
    };
    bsm_pins.push((2, thread.native_pin_roots.len()));
    thread.native_pin_roots.push(mt);
    bsm_args.push(Value::Object(Some(mt)));

    // 4. Static bootstrap args.
    // gci i33/sc: String arguments are recorded at their own entry, on `ldc`'s
    // terms, exactly as a condy's are (read once per bootstrap; `=0` is the
    // pre-i33 unrecorded form; the exact lone-surrogate units either way).
    let record_strings = crate::runtime::interpreter::constants::condy_string_arg_record_on();
    for sa in &static_args {
        let pos = bsm_args.len();
        let v = match sa {
            StaticArg::Int(i) => Value::Int(*i),
            StaticArg::Long(l) => Value::Long(*l),
            StaticArg::Float(f) => Value::Float(*f),
            StaticArg::Double(d) => Value::Double(*d),
            StaticArg::Str { cp_index, text } => {
                crate::runtime::interpreter::constants::condy_string_arg(
                    shared,
                    thread,
                    current_class_id,
                    *cp_index,
                    text,
                    info.fill_as_of,
                    record_strings,
                )?
            }
            StaticArg::WideStr { cp_index, units } => {
                crate::runtime::interpreter::constants::condy_wide_string_arg(
                    shared,
                    thread,
                    current_class_id,
                    *cp_index,
                    units,
                    info.fill_as_of,
                    record_strings,
                )?
            }
            StaticArg::Class(name) => {
                // Through the CALLER's defining loader, as the constant-pool
                // `Class` constant it is must be: `load_class_concurrent` asks
                // only the built-in chain, which cannot see a class of a
                // webapp / fat-jar / forked test loader. A miss is the Java
                // `NoClassDefFoundError` `ldc` of the same entry raises, not a
                // VM-internal class-not-found no handler can catch.
                let cid = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    thread,
                    current_class_id,
                    name,
                )
                .map_err(|e| {
                    crate::runtime::exceptions::convert_class_not_found(shared, thread, name, e)
                })?;
                Value::Object(Some(
                    crate::runtime::interpreter::constants::class_mirror_or_oom(
                        shared,
                        thread,
                        cid,
                        "indy-arg-class",
                    )?,
                ))
            }
            // `CONSTANT_MethodType` / `CONSTANT_MethodHandle` / `CONSTANT_Dynamic`:
            // resolved exactly as `ldc` of the same index resolves them, and
            // through the same per-entry record, so the two agree on identity
            // and a recorded failure is rethrown rather than re-resolved
            // (JVMS §5.4.3). A `LinkageError` from any of them propagates
            // unwrapped (JVMS §6.5: an `Error` is not wrapped in
            // `BootstrapMethodError`) and is then recorded against this
            // instruction by `settle_generic_link`, as HotSpot's
            // `save_and_throw_indy_exc` records it.
            StaticArg::MType {
                cp_index,
                descriptor,
            } => {
                let resolve = |t: &mut JvmThread| {
                    crate::runtime::interpreter::constants::resolve_method_type_constant(
                        shared,
                        t,
                        current_class_id,
                        descriptor,
                    )
                };
                // Permanent like `ldc` of the entry (nio21 wave 11 L4,
                // `constants::method_type_record_is_permanent`).
                let permanent =
                    crate::runtime::interpreter::constants::method_type_record_is_permanent(shared);
                resolve_recorded_static_arg(
                    shared,
                    thread,
                    current_class_id,
                    *cp_index,
                    permanent,
                    info.fill_as_of,
                    resolve,
                )?
            }
            StaticArg::MHandle { cp_index, handle } => {
                let resolve = |t: &mut JvmThread| {
                    crate::runtime::interpreter::constants::resolve_method_handle_constant(
                        shared,
                        t,
                        current_class_id,
                        handle.kind,
                        &handle.class_name,
                        &handle.member_name,
                        &handle.descriptor,
                    )
                };
                resolve_recorded_static_arg(
                    shared,
                    thread,
                    current_class_id,
                    *cp_index,
                    true,
                    info.fill_as_of,
                    resolve,
                )?
            }
            StaticArg::Dynamic(idx) => {
                crate::runtime::interpreter::constants::resolve_condy_constant(
                    shared,
                    thread,
                    current_class_id,
                    *idx,
                )?
            }
        };
        // Every reference is pinned the moment it exists: the next argument's
        // resolution can allocate, load classes and run Java.
        if let Value::Object(Some(o)) = v {
            bsm_pins.push((pos, thread.native_pin_roots.len()));
            thread.native_pin_roots.push(o);
        }
        bsm_args.push(v);
    }

    // Re-read object args from their (possibly forwarded) pin slots.
    for &(pos, slot) in &bsm_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            bsm_args[pos] = Value::Object(Some(o));
        }
    }
    // Interpreter round i6 wave 119, lane L4 (`CRATONVM_INDY_GENERIC_VIA_MAKESITE`,
    // default ON since the i6 wave-126 flip; `i38-L4-proposal-native-bootstraps-link-only-javac-shapes-natively`):
    // the generic JDK route hands the bootstrap to the JDK's own linkage,
    // `CallSite.makeSite(bsm, name, type, staticArgs, caller)` ->
    // `BootstrapMethodInvoker.invoke`, as HotSpot's `MethodHandleNatives.linkCallSite`
    // does, so the static-argument count, the `metafactory` fast path's casts, the
    // non-`CallSite` and target-type refusals and the `BootstrapMethodError`
    // wrapping are the JDK's own, not this function's copies below.
    // Wave 121 (host run of w119): not for `ObjectMethods.bootstrap`. Its
    // `toString` over a field getter plus an accessor METHOD
    // (`L4W40BootstrapTypedVarargs` `accessor-mixed-tostring`) failed through
    // `makeSite` with `RuntimeException: StringConcatException: Generator
    // failed`. The cause was not the route: the JDK `StringConcatFactory`'s
    // hidden-class constructor was refused by the native access check
    // (`i121-L4-jdk-string-concat-factory-hidden-class-constructor-is-refused`,
    // fixed under `CRATONVM_MH_HIDDEN_CLASS_RUNTIME_PACKAGE`). Wave 121 excluded
    // `ObjectMethods.bootstrap` from this arm; wave 124 dropped the exclusion
    // after the run lane found that page's probes SAME 16/16 on w121.
    if jdk_bytecode {
        let (bsm_ref_index, bsm_kind) = bsm_ref;
        let site = link_generic_via_make_site(
            shared,
            thread,
            &mut bsm_args,
            &bsm_pins,
            current_class_id,
            info,
            bsm_ref_index,
            bsm_kind,
            (bsm_class.as_str(), bsm_method.as_str(), bsm_desc.as_str()),
        );
        thread.native_pin_roots.truncate(pin_base);
        return site;
    }
    let last_is_array_or_null = match bsm_args.last() {
        Some(Value::Object(Some(o))) => {
            shared.mem.heap.kind_of(*o) == cratonvm_types::ObjectKind::Array
        }
        Some(Value::Object(None)) => true,
        _ => false,
    };
    // A trailing array of another reference type collects too
    // (`ObjectMethods.bootstrap`'s `MethodHandle... getters`; interpreter
    // round i1 wave 40, lane L4): `invokeWithArguments` of a varargs
    // bootstrap collects into the declared array type. It was passed the
    // bare first getter, so the JDK's own `ObjectMethods` route (the
    // `--real-jdk` answer for a site whose getters the native linkage does not
    // model) saw a `MethodHandle` where it reads `getters.length`.
    let typed_component = trailing_typed_array_component(&bsm_desc).map(str::to_string);
    let pack_from = bootstrap_varargs_pack_start(
        descriptor_param_count_and_last_is_object_array(&bsm_desc)
            .map(|(count, object_array)| (count, object_array || typed_component.is_some())),
        bsm_args.len(),
        last_is_array_or_null,
    );
    // The collecting array's component class, resolved before the packing
    // below takes the tail from the pin slots (the resolution can load a
    // class and move the heap; every reference is pinned).
    let packed_component: Option<ClassId> = match (pack_from, typed_component.as_deref()) {
        (Some(_), Some(name)) => Some(
            crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found(shared, thread, name, e)
            })?,
        ),
        _ => None,
    };
    if dbg_indy_all() {
        if let (Some(from), Some(name)) = (pack_from, typed_component.as_deref()) {
            eprintln!(
                "[indy-all] bootstrap {bsm_class}.{bsm_method}: collects {} trailing argument(s) into {name}[]",
                bsm_args.len() - from
            );
        }
    }

    // A primitive static argument (or a primitive condy value) bound to a
    // FIXED reference parameter is boxed, and one bound to a wider primitive
    // parameter is widened — the `asType` conversions `invokeWithArguments`
    // applies (JDK `BootstrapMethodInvoker`). They used to be passed raw, so a
    // bootstrap declaring `Object` received an `int` slot. Arguments a
    // trailing `Object[]` collects are boxed by the varargs packing below.
    let param_tags = parse_descriptor_args(&bsm_desc);
    let fixed_limit = pack_from.unwrap_or(usize::MAX);
    for pos in 3..bsm_args.len().min(param_tags.len()).min(fixed_limit) {
        match bootstrap_arg_fit(param_tags[pos], bsm_args[pos]) {
            BootstrapArgFit::Keep => {}
            BootstrapArgFit::Widen(v) => bsm_args[pos] = v,
            BootstrapArgFit::Box => {
                let boxed = box_bootstrap_primitive(shared, thread, bsm_args[pos])?;
                if let Value::Object(Some(o)) = boxed {
                    bsm_pins.push((pos, thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(o);
                }
                bsm_args[pos] = boxed;
            }
        }
    }

    // Re-read object args from their (possibly forwarded) pin slots.
    for &(pos, slot) in &bsm_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            bsm_args[pos] = Value::Object(Some(o));
        }
    }

    // `--real-jdk` (interpreter round i1 wave 41, lane L4): the casts
    // `BootstrapMethodInvoker`'s invocation makes, in argument order: a
    // static argument of another class than its parameter (or than the
    // component of a typed varargs array that collects it) is a
    // `ClassCastException` wrapped in `BootstrapMethodError`, and the
    // bootstrap never runs. It used to receive the value as it was: an
    // `Integer` in a `String` parameter, or stored into the `String[]` wave
    // 40's typed collection allocates. Probe
    // `L4W41BootstrapArgumentCasts.java`.
    if shared.classes.indy_link_checks {
        let (params, _) = crate::runtime::interpreter::split_method_descriptor_ref(&bsm_desc);
        let typed_param = typed_component.as_deref().map(|name| format!("L{name};"));
        for (pos, &value) in bsm_args.iter().enumerate().skip(3) {
            let param = if pos < fixed_limit {
                params.get(pos).copied()
            } else {
                typed_param.as_deref()
            };
            let Some(param) = param else {
                continue;
            };
            if let Some(message) = bootstrap_static_arg_cast_failure(shared, value, param) {
                if dbg_indy_all() {
                    eprintln!(
                        "[indy-all] bootstrap {bsm_class}.{bsm_method}: static argument {pos}: {message}"
                    );
                }
                return Err(bootstrap_method_error_with_new_cause(
                    shared,
                    thread,
                    "bootstrap method initialization exception",
                    "java/lang/ClassCastException",
                    &message,
                ));
            }
        }
    }

    // Some bootstrap methods declare a trailing `Object[]` formal parameter
    // that collects ALL "extra" constant-pool bootstrap arguments beyond the
    // fixed `(Lookup, String, MethodType)` prefix -- a JVMS-legal
    // invokedynamic linkage shape (mirrors a Java varargs bootstrap method).
    // JRuby 10.x's string-interpolation bootstrap uses exactly this shape:
    // `BuildDynamicStringSite.buildDString(Lookup, String, MethodType,
    // Object[])`. `bsm_args` above is built FLAT (`[lookup, name, mt,
    // static_arg_1, ..., static_arg_N]`) and `invoke_shared` sets up the
    // callee's locals positionally 1:1 -- so without this step, the 4th
    // formal parameter (the declared `Object[]`) receives `bsm_args[3]`
    // (the FIRST static bootstrap arg, a scalar) instead of an actual
    // array. `BuildDynamicStringSite`'s own constructor then computes
    // `bsmArgs.length - 6` as its metadata offset; CratonVM's
    // `arraylength`-of-non-array guard silently returns 0 for the scalar
    // (see `[GC-ARRAY-GUARD] array_length(non-array)`), so the offset goes
    // negative and the very next `aaload` throws
    // `ArrayIndexOutOfBoundsException` -- confirmed via `javap` decompile of
    // the real `jruby-base-10.0.2.0.jar` class plus
    // `JRubyScriptTemplateTests`'s `require 'ostruct'` (string
    // interpolation in `OpenStruct`'s class body, `ostruct.rb:477`). Pack
    // the excess trailing static args into a real `Object[]` (boxing any
    // primitive `Value`s -- `Object[]` elements must be references) to
    // match the declared descriptor before invoking. Which arguments are
    // packed is `bootstrap_varargs_pack_start`'s decision (the
    // `invokeWithArguments` rule for a varargs collector).
    {
        if let Some(leading) = pack_from {
            let tail_len = bsm_args.len() - leading;
            // Collecting and fallible (gc-common w4-c; see
            // `interpreter::native_alloc_collecting`). The tail is taken from
            // the pin slots only AFTER this allocation (below), so a collection
            // here cannot leave a stale reference in it.
            let arr = match packed_component {
                // A typed array (`MethodHandle[]`), as `invokeWithArguments`
                // collects it; `gc_alloc_array` collects and retries itself.
                Some(component) => match crate::runtime::interpreter::gc_alloc_array(
                    shared,
                    thread,
                    component,
                    cratonvm_types::ArrayElementType::Reference,
                    tail_len,
                ) {
                    Ok(arr) => arr,
                    Err(e) => {
                        thread.native_pin_roots.truncate(pin_base);
                        return Err(e);
                    }
                },
                None => match crate::runtime::interpreter::native_alloc_collecting(
                    shared,
                    thread,
                    "indy-varargs",
                    |shared, thread| {
                        let mut ctx = NativeContextImpl { shared, thread };
                        ctx.new_array(cratonvm_types::ArrayElementType::Reference, tail_len)
                    },
                ) {
                    Ok(arr) => arr,
                    Err(oom) => {
                        thread.native_pin_roots.truncate(pin_base);
                        return Err(oom.into());
                    }
                },
            };
            // GC-safety: the `valueOf` boxing calls below run arbitrary Java
            // (class-load + <clinit>) and can trigger a moving young GC;
            // pin the array AND every still-unprocessed object-typed tail
            // value, re-reading each from its pin slot right before use
            // (mirrors `bsm_pins` above, same function).
            let base_pin = thread.native_pin_roots.len();
            // `base_pin` -> the array itself.
            thread.native_pin_roots.push(arr);
            // The allocation above can move the heap: take the tail only now,
            // from the pin slots. It used to be copied before the allocation.
            for &(pos, slot) in &bsm_pins {
                if let Some(o) = thread.native_pin_roots.get(slot).copied() {
                    bsm_args[pos] = Value::Object(Some(o));
                }
            }
            let tail: Vec<Value> = bsm_args[leading..].to_vec();
            let mut tail_pins: Vec<Option<usize>> = Vec::with_capacity(tail.len());
            for v in &tail {
                if let Value::Object(Some(o)) = v {
                    tail_pins.push(Some(thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(*o);
                } else {
                    tail_pins.push(None);
                }
            }
            for (i, v) in tail.into_iter().enumerate() {
                let current_v = match tail_pins[i] {
                    Some(slot) => Value::Object(Some(thread.native_pin_roots[slot])),
                    None => v,
                };
                // A reference (String/Class/MethodType/null static arg, or a
                // value with no tail_pins slot) passes through unboxed.
                let boxed = box_bootstrap_primitive(shared, thread, current_v)?;
                let arr_now = thread.native_pin_roots[base_pin];
                let ctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                ctx.set_array_element(arr_now, i, boxed);
            }
            let arr_final = thread.native_pin_roots[base_pin];
            thread.native_pin_roots.truncate(base_pin);
            bsm_args.truncate(leading);
            bsm_args.push(Value::Object(Some(arr_final)));
        }
    }

    // --- Invoke the bootstrap method → CallSite. ---
    let dbg = dbg_indy_generic();
    if dbg {
        eprintln!(
            "[indy-generic] bootstrap {bsm_class}.{bsm_method} target={}{} bsm_args_len={}",
            info.target_name,
            info.target_descriptor,
            bsm_args.len()
        );
    }
    // A `REF_invokeStatic` bootstrap initializes the class that DECLARES it
    // (JVMS §5.5), never the named class of an inherited one:
    // `invoke_static_shared` loads the named class and initializes the
    // declaring one, as the condy route does
    // (`initialize_bootstrap_declaring_class`). Interpreter round i1 wave 15,
    // lane L4; `invoke_shared` initialized the named class.
    let callsite_val = if jdk_bytecode && bsm_is_static {
        // The JDK's own body, past any registered native (see
        // `bootstrap_generic_jdk`); it initializes the named class, which
        // declares every bootstrap this route takes.
        crate::vm::invoke_special_bytecode_only_shared(
            shared,
            thread,
            &bsm_class,
            &bsm_method,
            &bsm_desc,
            &bsm_args,
        )
    } else if bsm_is_static {
        crate::vm::invoke_static_shared_on_class(
            shared,
            thread,
            bsm_class_id.expect("resolved above when bsm_is_static && !jdk_bytecode"),
            &bsm_class,
            &bsm_method,
            &bsm_desc,
            &bsm_args,
        )
    } else {
        crate::vm::invoke_shared(
            shared,
            thread,
            &bsm_class,
            &bsm_method,
            &bsm_desc,
            &bsm_args,
        )
    };
    // Bootstrap args no longer needed -- released on the failure path too,
    // which the old `?` skipped, leaking every pin above into the caller.
    thread.native_pin_roots.truncate(pin_base);
    // JVMS §6.5 "Linking Exceptions": a bootstrap method that completes
    // abruptly with anything but an `Error` fails linkage with a
    // `BootstrapMethodError` whose cause is that exception. The raw exception
    // used to escape unwrapped.
    let callsite_val = callsite_val.map_err(|e| wrap_bootstrap_failure(shared, thread, e))?;
    if dbg {
        eprintln!("[indy-generic] bootstrap result = {callsite_val:?}");
    }

    let callsite = match callsite_val {
        Some(Value::Object(Some(cs))) => cs,
        // A bootstrap declared with a primitive return (interpreter round i5
        // wave 101, lane L4; probe
        // `L4W101IndyPrimitiveBootstrapResult.java`):
        // `BootstrapMethodInvoker.invoke` calls it through `invoke` with an
        // `Object` return, which boxes the answer BY THE DECLARED RETURN TYPE,
        // and then `CallSite.class.cast` refuses the box -- a
        // `ClassCastException` wrapped by `BootstrapMethodInvoker`. It was
        // reported as `makeSite`'s null answer ("failed to produce an
        // instance of CallSite", the other wrapper message).
        Some(prim @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))) => {
            let boxed = match (bsm_desc.rsplit(')').next(), prim) {
                (Some("Z"), _) => "java.lang.Boolean",
                (Some("B"), _) => "java.lang.Byte",
                (Some("C"), _) => "java.lang.Character",
                (Some("S"), _) => "java.lang.Short",
                (_, Value::Long(_)) => "java.lang.Long",
                (_, Value::Float(_)) => "java.lang.Float",
                (_, Value::Double(_)) => "java.lang.Double",
                _ => "java.lang.Integer",
            };
            return Err(bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "bootstrap method initialization exception",
                "java/lang/ClassCastException",
                &format!("Cannot cast {boxed} to java.lang.invoke.CallSite"),
            ));
        }
        // `CallSite.makeSite`: a null (or non-reference) answer is a
        // `ClassCastException` wrapped in `BootstrapMethodError`. This used to
        // be a VM-internal error no Java handler could catch.
        _ => {
            return Err(bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "CallSite bootstrap method initialization exception",
                "java/lang/ClassCastException",
                "CallSite bootstrap method failed to produce an instance of CallSite",
            ));
        }
    };
    // A non-null answer that is not a `CallSite` fails the
    // `CallSite.class.cast(result)` in `BootstrapMethodInvoker.invoke`. It used
    // to be accepted and `getTarget()` invoked on it.
    if let Some(failure) = non_callsite_bootstrap_result(shared, thread, callsite) {
        return Err(failure);
    }
    // `CallSite.makeSite`: the target's type must EQUAL the call site's
    // (interpreter round i1 wave 32, `--real-jdk`). An `asType`-compatible
    // target used to run.
    if shared.classes.indy_link_checks {
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(callsite);
        let mismatch = callsite_target_type_mismatch(
            shared,
            thread,
            current_class_id,
            callsite,
            &info.target_descriptor,
        );
        let callsite = thread.native_pin_roots[pin];
        thread.native_pin_roots.truncate(pin);
        if let Some(message) = mismatch {
            if dbg {
                eprintln!("[indy-generic] target type mismatch: {message}");
            }
            return Err(bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "CallSite bootstrap method initialization exception",
                "java/lang/invoke/WrongMethodTypeException",
                &message,
            ));
        }
        return Ok(callsite);
    }
    Ok(callsite)
}

/// [`link_generic_call_site`]'s `CRATONVM_INDY_GENERIC_VIA_MAKESITE` arm
/// (interpreter round i6 wave 119, lane L4): link through the JDK's
/// `CallSite.makeSite(MethodHandle bsm, String name, MethodType type, Object
/// staticArgs, Class<?> caller)`.
///
/// `bsm_args` is `[lookup, name, type, static args...]` as resolved (every
/// reference pinned at its `bsm_pins` slot, re-read here after each allocating
/// step). The bootstrap's handle is its `CONSTANT_MethodHandle` resolved as
/// `ldc` resolves it, through the per-entry record (one identity); the static
/// arguments are boxed into an `Object[]` (`null` for none, which
/// `BootstrapMethodInvoker` reads as no arguments); the caller is the class's
/// mirror. A throwable from `makeSite` is already HotSpot's: an `Error` (a
/// `BootstrapMethodError` included) passes [`wrap_bootstrap_failure`] unchanged.
/// The caller releases every pin above its own base.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::invokedynamic) fn link_generic_via_make_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    bsm_args: &mut [Value],
    bsm_pins: &[(usize, usize)],
    current_class_id: ClassId,
    info: &IndyInfo,
    bsm_ref_index: u16,
    bsm_kind: MethodHandleKind,
    (bsm_class, bsm_method, bsm_desc): (&str, &str, &str),
) -> Result<ObjectRef, MethodCallFailed> {
    let refresh = |thread: &JvmThread, bsm_args: &mut [Value]| {
        for &(pos, slot) in bsm_pins {
            if let Some(o) = thread.native_pin_roots.get(slot).copied() {
                bsm_args[pos] = Value::Object(Some(o));
            }
        }
    };
    if dbg_indy_all() {
        eprintln!(
            "[indy-all] cp-bsm#{bsm_ref_index}: {bsm_class}.{bsm_method}{bsm_desc} via CallSite.makeSite"
        );
    }
    // 1. The bootstrap's MethodHandle (a GC point: pinned, args re-read).
    let bsm = resolve_recorded_static_arg(
        shared,
        thread,
        current_class_id,
        bsm_ref_index,
        true,
        info.fill_as_of,
        |t| {
            crate::runtime::interpreter::constants::resolve_method_handle_constant(
                shared,
                t,
                current_class_id,
                bsm_kind,
                bsm_class,
                bsm_method,
                bsm_desc,
            )
        },
    )?;
    let Value::Object(Some(bsm)) = bsm else {
        return Err(VmError::Internal {
            message: format!("invokedynamic generic: bootstrap cp#{bsm_ref_index} is not a handle"),
        }
        .into());
    };
    let bsm_slot = thread.native_pin_roots.len();
    thread.native_pin_roots.push(bsm);
    refresh(thread, bsm_args);

    // 2. The static arguments, boxed, in an `Object[]` (`null` for none).
    let statics = bsm_args.len().saturating_sub(3);
    let info_slot = if statics == 0 {
        None
    } else {
        let arr = crate::runtime::interpreter::native_alloc_collecting(
            shared,
            thread,
            "indy-makesite-args",
            |shared, thread| {
                let mut ctx = NativeContextImpl { shared, thread };
                ctx.new_array(cratonvm_types::ArrayElementType::Reference, statics)
            },
        )
        .map_err(MethodCallFailed::from)?;
        let slot = thread.native_pin_roots.len();
        thread.native_pin_roots.push(arr);
        for i in 0..statics {
            refresh(thread, bsm_args);
            // `valueOf` runs Java: the array is re-read from its pin after it.
            let boxed = box_bootstrap_primitive(shared, thread, bsm_args[3 + i])?;
            let arr_now = thread.native_pin_roots[slot];
            let ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            ctx.set_array_element(arr_now, i, boxed);        }
        Some(slot)    };

    // 3. The caller's mirror.
    let caller = crate::runtime::interpreter::constants::class_mirror_or_oom(
        shared,
        thread,
        current_class_id,
        "indy-makesite-caller",
    )?;
    let caller_slot = thread.native_pin_roots.len();
    thread.native_pin_roots.push(caller);
    refresh(thread, bsm_args);

    let args = [
        Value::Object(Some(thread.native_pin_roots[bsm_slot])),
        bsm_args[1],
        bsm_args[2],
        match info_slot {
            Some(slot) => Value::Object(Some(thread.native_pin_roots[slot])),
            None => Value::Object(None),
        },
        Value::Object(Some(thread.native_pin_roots[caller_slot])),
    ];
    let result = crate::vm::invoke_static_shared(
        shared,
        thread,
        "java/lang/invoke/CallSite",
        "makeSite",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Object;Ljava/lang/Class;)Ljava/lang/invoke/CallSite;",
        &args,
    )
    .map_err(|e| wrap_bootstrap_failure(shared, thread, e))?;
    match result {
        Some(Value::Object(Some(site))) => Ok(site),
        other => Err(VmError::Internal {
            message: format!("invokedynamic generic: CallSite.makeSite answered {other:?}"),
        }
        .into()),
    }
}

/// `CallSite.makeSite`'s `wrongTargetType` message when the target of the
/// bootstrap's `callsite` is not typed exactly `site_descriptor`:
/// `MethodHandle()Object should be of type ()String`. `None` when the types
/// agree, and whenever a type cannot be read (no target, no `type`, a failed
/// `toMethodDescriptorString`): only a mismatch actually seen refuses a link.
pub(in crate::runtime::invokedynamic) fn callsite_target_type_mismatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    callsite: ObjectRef,
    site_descriptor: &str,
) -> Option<String> {
    let field = |obj: ObjectRef, name: &str| -> Option<ObjectRef> {
        let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(
            shared,
            shared.mem.heap.class_id_of(obj),
            name,
        )?;
        match shared.mem.heap.get_field(obj, slot) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    };
    // indy21c: `CallSite`'s own `target`, not a look-alike a subclass declares.
    let target_slot = call_site_target_slot(shared, shared.mem.heap.class_id_of(callsite))?;
    let target = match shared.mem.heap.get_field(callsite, target_slot) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let target_type = field(target, "type")?;
    let descriptor = match crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodType",
        "toMethodDescriptorString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(target_type))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => crate::vm::read_java_string(&shared.mem.heap, s)?,
        _ => return None,
    };
    if descriptor == site_descriptor {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    Some(format!(
        "MethodHandle{} should be of type {}",
        method_type_display_for(&cm, caller, &descriptor),
        method_type_display_for(&cm, caller, site_descriptor)
    ))
}

/// `MethodType.toString()` for a method descriptor: `(int,String)void`, each
/// type by its simple name.
pub(in crate::runtime::invokedynamic) fn method_type_display(descriptor: &str) -> String {
    method_type_display_with(descriptor, &|_| None)
}

/// [`method_type_display`] with each class the store holds for `caller`'s
/// loader named by `Class.getSimpleName()`, as `MethodType.toString()` names
/// it: the class's own `InnerClasses` entry's `inner_name` (`Local` for a
/// local `Outer$1Local`, `""` for an anonymous `Outer$1`), else everything
/// after the package (a top-level `A$B` keeps its `$`). A class that is not
/// loaded keeps the last-`$` text. Interpreter round i1 wave 39, lane L4
/// (item 2 of `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`).
pub(in crate::runtime::invokedynamic) fn method_type_display_for(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    descriptor: &str,
) -> String {
    method_type_display_with(descriptor, &|name| {
        let id = cm.find_class_by_name_for_class(name, caller)?;
        let class = cm.get_class(id)?;
        if class.is_hidden() {
            return None;
        }
        let own = class
            .inner_classes
            .iter()
            .find(|entry| entry.inner_class == name)
            .map(|entry| entry.inner_name.to_string());
        Some(own.unwrap_or_else(|| name.rsplit('/').next().unwrap_or(name).to_string()))
    })
}

pub(in crate::runtime::invokedynamic) fn method_type_display_with(descriptor: &str, loaded: &dyn Fn(&str) -> Option<String>) -> String {
    let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(descriptor);
    let simple = |token: &str| -> String {
        let dims = token.bytes().take_while(|&b| b == b'[').count();
        let base = &token[dims..];
        let mut out = match base.as_bytes().first() {
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'D') => "double".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'Z') => "boolean".to_string(),
            Some(b'V') => "void".to_string(),
            _ => {
                let name = base
                    .strip_prefix('L')
                    .and_then(|b| b.strip_suffix(';'))
                    .unwrap_or(base);
                loaded(name).unwrap_or_else(|| {
                    let name = name.rsplit('/').next().unwrap_or(name);
                    name.rsplit('$').next().unwrap_or(name).to_string()
                })
            }
        };
        for _ in 0..dims {
            out.push_str("[]");
        }
        out
    };
    let params: Vec<String> = params.iter().map(|p| simple(p)).collect();
    format!("({}){}", params.join(","), simple(ret))
}

/// indy21c: the instance-field slot of `java.lang.invoke.CallSite.target` for a
/// call site of class `call_site_class`.
///
/// The by-name walk ([`crate::vm::vm_exec::resolve_field_slot_by_name_cached`])
/// is most-derived-first, so a user subclass of `MutableCallSite` /
/// `VolatileCallSite` / `ConstantCallSite` that declares an instance field of
/// its own named `target` hides `CallSite.target` from it. HotSpot reads
/// `CallSite.target` at a fixed offset (`getTarget` and the linker never see the
/// look-alike), so the interpreter's indy path, the bridge's direct memo and
/// the compiled guard must read `CallSite`'s own slot. A subclass's slots come
/// after every superclass slot, so `CallSite`'s slot, when strictly smaller
/// than the derived one, is the real field's; otherwise (no shadow, or
/// `CallSite` not loaded yet) the derived answer, as before. The `CallSite`
/// class id is memoized per VM (`JitBridgeState::call_site_class`): no
/// class-manager lock after the first read.
pub(crate) fn call_site_target_slot(shared: &SharedVm, call_site_class: ClassId) -> Option<usize> {
    use crate::vm::vm_exec::resolve_field_slot_by_name_cached as slot_of;
    let derived = slot_of(shared, call_site_class, "target");
    let call_site = match shared.jit.bridge.call_site_class.get() {
        Some(&known) => Some(known),
        None => {
            let found = shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id("java/lang/invoke/CallSite");
            if let Some(found) = found {
                let _ = shared.jit.bridge.call_site_class.set(found);
            }
            found
        }
    };
    let own = call_site.and_then(|cs| slot_of(shared, cs, "target"));
    match (own, derived) {
        (Some(own), Some(derived)) if own < derived => Some(own),
        _ => derived,
    }
}

/// The current target of `call_site`: its `target` field, the slot every
/// `CallSite` native in `lang_invoke` (`cs_target_slot`) reads and writes, so a
/// `setTarget` is observed on the very next execution. `getTarget()` is final
/// on all three JDK subclasses and returns that field. Only when the field
/// cannot be resolved is `getTarget()` actually invoked.
///
/// `call_site` must be pinned at `pin_slot`; it is re-read from there.
pub(in crate::runtime::invokedynamic) fn call_site_target(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pin_slot: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let call_site = thread.native_pin_roots[pin_slot];
    let class_id = shared.mem.heap.class_id_of(call_site);
    if let Some(slot) = call_site_target_slot(shared, class_id) {
        return Ok(match shared.mem.heap.get_field(call_site, slot) {
            Value::Object(target) => target,
            _ => None,
        });
    }
    let target = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/CallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(call_site))],
    )?;
    Ok(match target {
        Some(Value::Object(target)) => target,
        _ => None,
    })
}

/// The parameter type tags of `target_mh`'s own `type()` (its `MethodType`
/// field, the adapted type of a bound or converted handle), or `None` when
/// the handle has no readable type. Pure reads: no allocation on the Java
/// heap, no safepoint. Interpreter round i6 wave 134, lane L2c
/// ([`invoke_generic_call_site_with`]'s `Z` operands).
pub(in crate::runtime::invokedynamic) fn target_handle_param_types(
    shared: &SharedVm,
    thread: &mut JvmThread,
    target_mh: ObjectRef,
) -> Option<Vec<char>> {
    let ctx = NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    let Value::Object(Some(mt)) = ctx.get_field_by_name(target_mh, "type") else {
        return None;
    };
    let descriptor =
        cratonvm_native_builtins::lang_invoke::method_type_object_descriptor(&ctx, mt)?;
    Some(parse_descriptor_args(&descriptor))
}

/// `Boolean.TRUE` / `Boolean.FALSE` — the objects `Boolean.valueOf(boolean)`
/// returns — read from `Boolean`'s statics, or `None` when the class is not
/// loaded or its `<clinit>` has not stored them yet (the slot then holds a
/// default, never an object). The same resolution as
/// `vm_exec::proxy_canonical_boolean`, minus its `load_class` WRITE lock: an
/// indy operand typed `Z` does not need `Boolean` loaded to exist, and the
/// caller falls back to the Java call. Takes one read lock, runs no Java,
/// allocates nothing, and is not a safepoint.
pub(in crate::runtime::invokedynamic) fn canonical_boolean(shared: &SharedVm, truthy: bool) -> Option<ObjectRef> {
    let want = if truthy { "TRUE" } else { "FALSE" };
    let (class_id, static_index) = {
        let cm = shared.classes.class_manager.read();
        let class_id = cm.get_loaded_class_id("java/lang/Boolean")?;
        // Static field INDEX (position among the static fields only), which
        // is what `get_static_shared` is indexed by.
        let static_index = cm
            .get_class(class_id)?
            .fields
            .iter()
            .filter(|f| f.is_static())
            .position(|f| &*f.name == want)?;
        (class_id, static_index)
    };
    match crate::vm::get_static_shared(shared, class_id, static_index) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Invoke the current target of a linked generic `CallSite` with the
/// instruction's operands, and push the result.
///
/// `call_site` arrives unpinned (see `settle_generic_link`); it is pinned before
/// the first safepoint ([`invoke_generic_call_site_with`]).
pub(in crate::runtime::invokedynamic) fn invoke_generic_call_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    call_site: ObjectRef,
    shape: &GenericIndyShape,
) -> Result<(), MethodCallFailed> {
    // --- Pop the dynamic call arguments (descriptor-typed). ---
    //
    // JIT round 16 wave 7 (lane indy): popped BEFORE the target is read (it
    // used to be after), so the invocation below is a function of values that
    // the compiled bridge can call with its own operands. Popping is not a
    // safepoint, and the core pins every reference operand before its first
    // one, so the operands stay rooted exactly as long as they were.
    let arg_types = &shape.arg_types;
    if dbg_indy_generic() {
        eprintln!(
            "[indy-generic] about to pop {} dyn args; stack depth before pop = {}",
            arg_types.len(),
            thread.frames[frame_idx].stack.len(),
        );
    }
    // Inline storage: a call site with up to 8 operands (every Groovy / JRuby
    // shape seen) allocates none of these lists.
    let mut dyn_args: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(arg_types.len());
    for i in 0..arg_types.len() {
        let cv = thread.frames[frame_idx].stack.pop_compact();
        let desc_byte = arg_types
            .get(arg_types.len() - 1 - i)
            .copied()
            .unwrap_or('L') as u8;
        dyn_args.push(cv.decode_by_descriptor(desc_byte));
    }
    dyn_args.reverse();
    // --- Push the result, coerced to the call-site return type. ---
    if let Some(coerced) =
        invoke_generic_call_site_with(shared, thread, call_site, shape, &dyn_args)?
    {
        thread.frames[frame_idx].stack.push(coerced)?;
    }
    Ok(())
}

/// The frame-independent body of [`invoke_generic_call_site`]: invoke the
/// current target of the linked `call_site` with `args` (the instruction's
/// operands in source order, already descriptor-typed) and answer the result
/// coerced to the site's return type, or `None` for a `void` site.
///
/// The interpreter's executor and the compiled bridge
/// (`CRATONVM_JIT_INDY_LINKED_CALL_SITE`, `execute_jit_indy_generic_raw`) both
/// invoke through here. `call_site` and every reference in `args` arrive
/// unpinned and are pinned before the first safepoint: neither the operand
/// stack they were popped from nor the compiled caller's spill buffer is a
/// root this function can count on.
pub(in crate::runtime::invokedynamic) fn invoke_generic_call_site_with(
    shared: &SharedVm,
    thread: &mut JvmThread,
    call_site: ObjectRef,
    shape: &GenericIndyShape,
    args: &[Value],
) -> Result<Option<Value>, MethodCallFailed> {
    let dbg = dbg_indy_generic();
    let arg_types = &shape.arg_types;
    let pin_base = thread.native_pin_roots.len();
    let mut dyn_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::from_slice(args);
    // Pin the reference operands FIRST: reading the target can run
    // `CallSite.getTarget` (a safepoint).
    let mut dyn_pins: smallvec::SmallVec<[(usize, usize); 8]> = smallvec::SmallVec::new();
    for (i, v) in dyn_args.iter().enumerate() {
        if let Value::Object(Some(o)) = v {
            dyn_pins.push((i, thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        }
    }
    let call_site_slot = thread.native_pin_roots.len();
    thread.native_pin_roots.push(call_site);
    let target_mh = call_site_target(shared, thread, call_site_slot);
    let target_mh = match target_mh {
        Ok(Some(mh)) => mh,
        Ok(None) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(VmError::Internal {
                message: format!(
                    "invokedynamic generic: CallSite for {} has a null target",
                    shape.target_descriptor
                ),
            }
            .into());
        }
        Err(e) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e);
        }
    };
    // Pin the target across the invocation. (The call site stays pinned below
    // it until the common truncate: it is not needed again, and a pin is
    // cheaper than shuffling the operand pins.)
    let mh_slot = thread.native_pin_roots.len();
    thread.native_pin_roots.push(target_mh);
    // The operands as they are NOW: `CallSite.getTarget` may have moved them.
    for &(i, slot) in &dyn_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            dyn_args[i] = Value::Object(Some(o));
        }
    }
    if dyn_args.len() != arg_types.len() {
        thread.native_pin_roots.truncate(pin_base);
        return Err(VmError::Internal {
            message: format!(
                "invokedynamic generic: {} operands for descriptor {}",
                dyn_args.len(),
                shape.target_descriptor
            ),
        }
        .into());
    }

    // --- Invoke the target: MethodHandle.invoke(args...). The signature-
    //     polymorphic native reads the real descriptor off the handle and
    //     adapts each spread argument, so we pass the dynamic args directly. ---
    let mut invoke_args: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(dyn_args.len() + 1);
    // Generic Groovy indy targets are ultimately invoked through our Object[]
    // MethodHandle bridge. Without an explicit conversion here, a call-site
    // `Z` value is represented as `Value::Int` and that bridge boxes it as an
    // Integer. Groovy's constructor selector then rejects a real boolean
    // parameter (LayoutDialect's DecorateProcessor is the concrete witness).
    // Box boolean slots as Boolean before entering the erased bridge; a typed
    // handle will unbox it again, while a dynamic Groovy target observes the
    // correct Boolean runtime class.
    //
    // Interpreter round i6 wave 134, lane L2c (`CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z`,
    // HotSpot parity, default ON, `=0` boxes every `Z` as before; page
    // `docs/feature-designs/jit-ir-optimizations.md`):
    // a `Z` operand whose position in the TARGET handle's own `type()` is the
    // primitive `boolean` stays the `int` the call site pushed. `invoke` of a
    // handle at the call site's own type converts nothing (HotSpot's
    // `asType` identity), and the box reached a COMPILED target's `boolean`
    // parameter as its address -- non-zero, i.e. `true` for `false`
    // (`L2cW134GenericIndyFalseArm` `flag=..7/true`; `L2bW133GenericIndyThreadRow`
    // phase1 1196 / phase2 1200 mismatches). An `Object` / `Boolean` /
    // collector position (Groovy's selectors) is boxed exactly as before.
    let target_params: Option<Vec<char>> =
        if arg_types.contains(&'Z') {
            let mh = thread.native_pin_roots[mh_slot];
            target_handle_param_types(shared, thread, mh)
                .filter(|params| params.len() == arg_types.len())
        } else {
            None
        };
    for (i, ty) in arg_types.iter().enumerate() {
        if *ty == 'Z' {
            if target_params
                .as_ref()
                .is_some_and(|params| params.get(i) == Some(&'Z'))
            {
                continue;
            }
            if let Value::Int(v) = dyn_args[i] {
                // `Boolean.valueOf(b)` is `b ? TRUE : FALSE`: read the static
                // instead of making an interpreted call (two per Groovy
                // constructor site, whose trailing flags are `Z, Z`). Reading
                // a static is not a safepoint. The Java call stays as the
                // fallback before `Boolean` is initialized.
                if let Some(o) = canonical_boolean(shared, v != 0) {
                    // Pinned like any operand: a later `Z` slot's fallback
                    // call below is a safepoint.
                    dyn_pins.push((i, thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(o);
                    dyn_args[i] = Value::Object(Some(o));
                    continue;
                }
                let boxed = match crate::vm::invoke_shared(
                    shared,
                    thread,
                    "java/lang/Boolean",
                    "valueOf",
                    "(Z)Ljava/lang/Boolean;",
                    &[Value::Int(v)],
                ) {
                    Ok(boxed) => boxed.unwrap_or(Value::Object(None)),
                    Err(e) => {
                        thread.native_pin_roots.truncate(pin_base);
                        return Err(e);
                    }
                };
                if let Value::Object(Some(o)) = boxed {
                    dyn_pins.push((i, thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(o);
                }
                dyn_args[i] = boxed;
            }
        }
    }
    // `MethodHandle`'s class, resolved once per linked site (stage 2a, wave
    // 16): `invoke_shared` by name re-probed the loaded-class table and
    // pinned every argument a second time on each execution, though every
    // reference here is already pinned. Its first resolution can load, a
    // safepoint, so it comes before the refresh below.
    let method_handle_class = match shape.method_handle_class(shared) {
        Ok(id) => id,
        Err(e) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e.into());
        }
    };
    // Re-read every reference argument from its pin AFTER the boxing loop
    // above: `Boolean.valueOf` is a Java call (and on first use a `<clinit>`),
    // i.e. a safepoint, so a refresh taken before it -- where this loop used
    // to sit -- could hand the target pre-move addresses.
    for &(i, slot) in &dyn_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            dyn_args[i] = Value::Object(Some(o));
        }
    }
    let target_mh = thread.native_pin_roots[mh_slot];
    invoke_args.push(Value::Object(Some(target_mh)));
    invoke_args.extend_from_slice(&dyn_args);
    if dbg {
        eprintln!(
            "[indy-generic] invoking target MH with {} dyn args: {:?}",
            dyn_args.len(),
            dyn_args
        );
    }
    // Publish the call-site descriptor, as the `invoke_or_native` door does for a
    // `MethodHandle.invoke` the bytecode wrote: the `ClassId`-resolved route
    // below does not, and without it a collector (`asCollector`, the shape
    // Groovy's indy `selectMethod` handle is) boxes the raw `Z` operand by its
    // `Value` kind -- an `Integer` -- where the call site wrote `boolean`
    // (`E.m(true)` in a Groovy script: `ClassCastException: Cannot cast
    // java.lang.Integer to java.lang.Boolean`; `ThymeleafServletAutoConfigurationTests`).
    // Armed immediately before the dispatch: anything in between that itself
    // dispatches a handle would consume it.
    cratonvm_native_api::poly_call_site::arm(&shape.target_descriptor);
    // Interpreter round i6 wave 117, lane L4 (`CRATONVM_INDY_GENERIC_DIRECT_INVOKE`,
    // default ON since the wave-129 triage, `=0` off; stage 2 of
    // `i2-L6-proposal-generic-indy-invoke-path`): the
    // native that dispatch walk ends in, called directly -- the registry's
    // `MethodHandle.invoke([Object])Object` bridge (looked up per call, so a
    // re-registration is followed), with the site descriptor armed above, and
    // the block's poly-return unboxing. Only for a target the block itself
    // would hand that bridge ([`GenericIndyShape::direct_invoke_admits`]);
    // anything else, or no registered bridge, takes the walk.
    if shape.direct_invoke_admits(shared, target_mh) {
        // Wave 119: the bridge's registry id, resolved once per generation.
        if let Some(callback) = shape.direct_invoke_callback(shared) {
            let raw = crate::vm::safe_native_call(shared, thread, callback, &invoke_args);
            let result = raw.and_then(|value| {
                crate::vm::unbox_poly_return_checked(
                    shared,
                    thread,
                    value,
                    &shape.target_descriptor,
                    "invoke",
                )
            });
            thread.native_pin_roots.truncate(pin_base);
            let result = result?;
            if shape.ret_byte == b'V' {
                return Ok(None);
            }
            let val = result.unwrap_or(Value::Object(None));
            return Ok(Some(crate::vm::coerce_value_against_ret_char(
                val,
                shape.ret_byte,
                shared,
            )));
        }
    }
    // `invoke_shared`'s own tail, from the resolved class: the references stay
    // pinned (above) until the call returns.
    let result = crate::vm::invoke_by_class_id_shared(
        shared,
        thread,
        method_handle_class,
        "invoke",
        // MethodHandle.invoke is signature-polymorphic. Preserve the actual
        // invokedynamic descriptor so primitive call-site arguments (notably
        // Groovy's trailing `Z, Z` constructor flags) are adapted as their
        // declared primitive types instead of being erased and boxed as
        // Integer through an artificial Object[] signature.
        &shape.target_descriptor,
        &invoke_args,
    );
    thread.native_pin_roots.truncate(pin_base);
    let result = result?;
    if dbg {
        eprintln!("[indy-generic] target MH invoke result = {result:?}");
    }
    if shape.ret_byte == b'V' {
        return Ok(None);
    }
    let val = result.unwrap_or(Value::Object(None));
    Ok(Some(crate::vm::coerce_value_against_ret_char(
        val,
        shape.ret_byte,
        shared,
    )))
}

/// The linkage failure for a bootstrap method that completed abruptly, per
/// JVMS §6.5 `invokedynamic` "Linking Exceptions" and the JDK's
/// `BootstrapMethodInvoker.invoke`: an `Error` (including a
/// `BootstrapMethodError` the bootstrap itself threw) passes through
/// unchanged; any other throwable becomes the cause of a new
/// `BootstrapMethodError("bootstrap method initialization exception")`.
/// Non-Java failures (`InternalError(..)`) are returned as they are.
#[cold]
pub(in crate::runtime::invokedynamic) fn wrap_bootstrap_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    let cause = match failure {
        MethodCallFailed::ExceptionThrown(cause) => cause,
        other => return other,
    };
    let is_error = {
        let cm = shared.classes.class_manager.read();
        let cause_cid = shared.mem.heap.class_id_of(cause);
        cm.get_loaded_class_id("java/lang/Error")
            .is_some_and(|error_cid| cm.is_subclass_of(cause_cid, error_cid))
    };
    if is_error {
        return MethodCallFailed::ExceptionThrown(cause);
    }
    bootstrap_method_error(
        shared,
        thread,
        "bootstrap method initialization exception",
        cause,
    )
}

/// A new `BootstrapMethodError(message)` whose cause is `cause`.
///
/// Falls back to throwing `cause` itself if the error object cannot be built:
/// losing the wrapper is better than losing the original failure.
#[cold]
pub(in crate::runtime::invokedynamic) fn bootstrap_method_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: &str,
    cause: ObjectRef,
) -> MethodCallFailed {
    throwable_with_cause(
        shared,
        thread,
        "java/lang/BootstrapMethodError",
        message,
        cause,
    )
}

/// A new `class(message)` whose cause is `cause` ([`bootstrap_method_error`]
/// for any throwable class; wave 39 uses it for an `IllegalAccessError`).
pub(crate) fn throwable_with_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class: &str,
    message: &str,
    cause: ObjectRef,
) -> MethodCallFailed {
    // `cause` is pinned across the two Java calls below, both safepoints.
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(cause);
    let created =
        crate::runtime::exceptions::create_exception_object(shared, thread, class, Some(message));
    let bme = match created {
        Ok(bme) => bme,
        Err(_) => {
            let cause = thread.native_pin_roots[pin];
            thread.native_pin_roots.truncate(pin);
            return MethodCallFailed::ExceptionThrown(cause);
        }
    };
    thread.native_pin_roots.push(bme);
    let cause = thread.native_pin_roots[pin];
    // The `(String)` constructor leaves `cause == this`, which is exactly the
    // state `initCause` accepts. A failure here leaves a cause-less error,
    // which is still the right class and message.
    let _ = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(bme)), Value::Object(Some(cause))],
    );
    let bme = thread.native_pin_roots[pin + 1];
    thread.native_pin_roots.truncate(pin);
    MethodCallFailed::ExceptionThrown(bme)
}

/// [`bootstrap_method_error`] over a freshly created `cause_class(cause_message)`.
#[cold]
pub(in crate::runtime::invokedynamic) fn bootstrap_method_error_with_new_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: &str,
    cause_class: &str,
    cause_message: &str,
) -> MethodCallFailed {
    bootstrap_method_error_with_new_cause_opt(
        shared,
        thread,
        message,
        cause_class,
        Some(cause_message),
    )
}

/// [`bootstrap_method_error_with_new_cause`] with a cause whose message may
/// be null (`Objects.requireNonNull`'s `NullPointerException`; wave 41).
pub(in crate::runtime::invokedynamic) fn bootstrap_method_error_with_new_cause_opt(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: &str,
    cause_class: &str,
    cause_message: Option<&str>,
) -> MethodCallFailed {
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        cause_class,
        cause_message,
    ) {
        Ok(cause) => bootstrap_method_error(shared, thread, message, cause),
        Err(e) => e,
    }
}

/// `Some(failure)` when a bootstrap answered a non-null object that is not a
/// `java.lang.invoke.CallSite`: the `ClassCastException` from
/// `CallSite.class.cast(result)`, wrapped as `BootstrapMethodInvoker` wraps it.
/// `None` when it is a `CallSite`, or when `CallSite` itself is not loaded
/// (then nothing can be concluded and the caller proceeds as before).
pub(in crate::runtime::invokedynamic) fn non_callsite_bootstrap_result(
    shared: &SharedVm,
    thread: &mut JvmThread,
    result: ObjectRef,
) -> Option<MethodCallFailed> {
    let actual = {
        let cm = shared.classes.class_manager.read();
        let callsite_cid = cm.get_loaded_class_id("java/lang/invoke/CallSite")?;
        let result_cid = shared.mem.heap.class_id_of(result);
        if cm.is_subclass_of(result_cid, callsite_cid) {
            return None;
        }
        cm.get_class(result_cid)
            .map(|c| c.name.replace('/', "."))
            .unwrap_or_else(|| "?".to_string())
    };
    Some(bootstrap_method_error_with_new_cause(
        shared,
        thread,
        "bootstrap method initialization exception",
        "java/lang/ClassCastException",
        &format!("Cannot cast {actual} to java.lang.invoke.CallSite"),
    ))
}

/// Raise `java.lang.BootstrapMethodError` for a bootstrap method outside the
/// supported set (StringConcatFactory, LambdaMetafactory, SwitchBootstraps,
/// ObjectMethods).
///
/// Per JVMS §5.4.3.6, when the bootstrap method invocation completes
/// abnormally (here: it is not implemented), the linkage of the `invokedynamic`
/// instruction throws `BootstrapMethodError` carrying the original failure as
/// its cause. We do not have a Java `Throwable` cause to wrap, so the detail
/// message identifies the offending bootstrap method and call site. This is a
/// loud, catchable error — callers that wrap `invokedynamic` in
/// `catch (Throwable)` / `catch (Error)` observe it normally — instead of a
/// silent wrong value that corrupts the caller's computation.
#[cold]
#[allow(dead_code)] // retained as a loud fallback for future BSM gating
pub(in crate::runtime::invokedynamic) fn raise_bootstrap_method_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    info: &IndyInfo,
    caller: &str,
) -> Result<(), MethodCallFailed> {
    let msg = format!(
        "unsupported bootstrap method {}.{} for call site {}{} at {}",
        info.bsm_class, info.bsm_method, info.target_name, info.target_descriptor, caller
    );
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/BootstrapMethodError",
        Some(&msg),
    ) {
        Ok(obj_ref) => Err(MethodCallFailed::ExceptionThrown(obj_ref)),
        // If we cannot even construct the Java error object (e.g. rt.jar absent),
        // surface a loud internal error rather than falling back to a silent
        // null/zero push — the whole point of this path is to never return a
        // wrong value to the caller.
        Err(_) => Err(MethodCallFailed::InternalError(VmError::Internal {
            message: msg,
        })),
    }
}

/// Execute a previously cached call site (fast path).
pub(in crate::runtime::invokedynamic) fn execute_cached_call_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: &ResolvedCallSite,
) -> Result<(), MethodCallFailed> {
    match site {
        ResolvedCallSite::StringConcat {
            recipe,
            constant_args,
            target_descriptor,
            latin1_shape,
        } => execute_string_concat(
            shared,
            thread,
            frame_idx,
            recipe,
            constant_args.as_slice(),
            target_descriptor,
            Some(*latin1_shape),
        ),
        ResolvedCallSite::Lambda(lcs) => execute_cached_lambda(shared, thread, frame_idx, lcs),
        ResolvedCallSite::TypeSwitch { labels } => {
            execute_type_switch(shared, thread, frame_idx, labels)
        }
        ResolvedCallSite::EnumSwitch { labels } => {
            execute_enum_switch(shared, thread, frame_idx, labels)
        }
        ResolvedCallSite::RecordObjectMethod {
            method,
            component_names,
            field_indices,
            field_descriptors,
            getter_driven,
            accessor_getters,
        } => execute_record_object_method(
            shared,
            thread,
            frame_idx,
            *method,
            component_names,
            field_indices,
            field_descriptors,
            *getter_driven,
            accessor_getters,
        ),
    }
}

/// A lambda's implementation handle, resolved far enough to raise what its
/// resolution raises: the owner through the caller's loader (a
/// `NoClassDefFoundError` when it cannot be loaded), then the member, which is
/// a `NoSuchMethodError` only when it is provably absent
/// (`selection::member_provably_absent`: a hierarchy with real class bytes,
/// not a signature-polymorphic owner). Anything the store cannot decide is
/// left to the call, as before.
pub(in crate::runtime::invokedynamic) fn lambda_impl_resolves(
    shared: &SharedVm,
    thread: &mut JvmThread,
    current_class_id: ClassId,
    impl_handle: &MethodHandle,
) -> Result<(), MethodCallFailed> {
    let owner_name: &str = &impl_handle.class_name;
    // A field handle names no method: `member_provably_absent` would call its
    // field a missing method (a `NoSuchMethodError` HotSpot never raises).
    if owner_name.starts_with('[')
        || matches!(
            impl_handle.kind,
            MethodHandleKind::GetField
                | MethodHandleKind::GetStatic
                | MethodHandleKind::PutField
                | MethodHandleKind::PutStatic
        )
    {
        return Ok(());
    }
    let owner = crate::runtime::interpreter::resolve_class_loader_aware(
        shared,
        thread,
        current_class_id,
        owner_name,
    )
    .map_err(|e| {
        crate::runtime::exceptions::convert_class_not_found(shared, thread, owner_name, e)
    })?;
    let absent = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::resolve::selection::member_provably_absent(
            &cm.class_store,
            owner,
            &impl_handle.member_name,
            &impl_handle.descriptor,
        )
    };
    if absent {
        return Err(crate::error::LinkageError::NoSuchMethodError {
            class_name: owner_name.to_string(),
            method_name: impl_handle.member_name.to_string(),
            method_descriptor: impl_handle.descriptor.to_string(),
        }
        .into());
    }
    Ok(())
}

/// Bootstrap a LambdaMetafactory.metafactory call site.
///
/// LambdaMetafactory bootstrap arguments:
///   arg[0]: MethodType — SAM erased method type
///   arg[1]: MethodHandle — implementation method
///   arg[2]: MethodType — SAM instantiated method type
///
/// The invokedynamic descriptor specifies the captured values → functional interface type.
pub(in crate::runtime::invokedynamic) fn bootstrap_lambda(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    singleton_site: Option<(u64, usize)>,
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Parse bootstrap arguments from constant pool (a `MethodType` a dynamic
    // constant answered at position 0 or 2 is read from its value; wave 41)
    // We need to re-acquire the class manager lock briefly to resolve the BSM args
    let (sam_erased_desc, impl_handle, instantiated_desc, host_loader) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "lambda bootstrap: class not found".to_string(),
            })?;

        if info.bootstrap_arg_indices.len() < 3 {
            return Err(VmError::Internal {
                message: format!(
                    "LambdaMetafactory: expected 3 bootstrap args, got {}",
                    info.bootstrap_arg_indices.len()
                ),
            }
            .into());
        }

        let args = &info.bootstrap_arg_indices;
        let sam_erased =
            lambda_method_type_arg(&class.constant_pool, args, condy, 0).ok_or_else(|| {
                VmError::Internal {
                    message: "LambdaMetafactory: invalid SAM erased MethodType".to_string(),
                }
            })?;

        // A dynamic constant's direct handle is read from its value (wave 42).
        let impl_mh = match condy.get(1) {
            Some(Some(LambdaCondyArg {
                method_handle: Some(handle),
                ..
            })) => handle.clone(),
            _ => resolve_method_handle_full(&class.constant_pool, info.bootstrap_arg_indices[1])?,
        };

        let instantiated = lambda_method_type_arg(&class.constant_pool, args, condy, 2)
            .ok_or_else(|| VmError::Internal {
                message: "LambdaMetafactory: invalid instantiated MethodType".to_string(),
            })?;

        (sam_erased, impl_mh, instantiated, class.loader_id)
    };

    // Parse the factory descriptor to determine:
    //   - capture types (parameters of the invokedynamic)
    //   - functional interface (return type)
    // Under `--real-jdk` the implementation handle was resolved by the caller
    // (`lambda_impl_resolves`, interpreter round i1 wave 33; moved before the
    // shape checks in wave 37, as HotSpot resolves static arguments before it
    // invokes the bootstrap).

    let capture_types = parse_descriptor_args(&info.target_descriptor);
    let functional_interface =
        parse_return_type_class(&info.target_descriptor).ok_or_else(|| VmError::Internal {
            message: format!(
                "LambdaMetafactory: cannot parse return type from '{}'",
                info.target_descriptor
            ),
        })?;

    // Resolve the functional interface through the HOST class's loader.
    // A name-only lookup at dispatch time picks an arbitrary copy when two
    // loaders define the same interface (forked-classloader tests re-define
    // the whole framework); default methods would then execute in the wrong
    // loader's context and produce objects failing cross-loader identity
    // checks (Spring AOT `ArgumentCodeGenerator.and()` → javapoet
    // `TypeName.equals` getClass() mismatch).
    //
    // JIT r25 wave 6 (W6-R fix 2): RESOLVED, not looked up. HotSpot resolves
    // the call site's `MethodType` (whose return type is the interface)
    // through the host class's defining loader, loading it if need be. The
    // lookup alone sees only classes already loaded: in a child-first loader
    // whose own `p.Fn` was not loaded yet (a plain lambda never casts to it),
    // it fell through the parent chain to the APP loader's `p.Fn`, and since
    // `lambda_proxy_satisfies` answers a resolved interface by identity, the
    // proxy then failed the `checkcast` to its own loader's `p.Fn` (probe
    // `R25W6ProxyWaysLoaders`, `makeSer`). Same door as the markers below; a
    // name that does not resolve keeps the old lookup (and the by-name rules
    // that answer a `None`).
    let functional_interface_id = crate::runtime::interpreter::resolve_class_loader_aware(
        shared,
        &mut *thread,
        current_class_id,
        &functional_interface,
    )
    .ok()
    .filter(|cid| *cid != ClassId::new(0))
    .or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id_for_requester(&functional_interface, host_loader)
    });

    if dbg_lambda_dispatch() {
        eprintln!(
            "[DBG_LAMBDA] bootstrap host={:?} loader={:?} iface={} resolved_id={:?}",
            current_class_id, host_loader, functional_interface, functional_interface_id
        );
    }
    // `altMetafactory` carries extra bootstrap static arguments beyond
    // `metafactory`'s three; the fourth is an int bitmask whose
    // `FLAG_SERIALIZABLE` (0x1) bit is set when the source target type was
    // `Serializable`-intersected. Real HotSpot spins a `writeReplace()` on the
    // proxy -- and adds `java.io.Serializable` to its interface list -- only
    // for those lambdas; an ordinary `Supplier<String> s = () -> "x"` gets
    // neither, and `(Serializable) s` throws ClassCastException. Record the bit
    // so the reflective surfaces can tell the two apart. A plain `metafactory`
    // site has no flags word and is never serializable BY FLAG (it can still be
    // serializable by inheritance -- see `SharedVm::lambda_proxy_serializability`).
    //
    // The WHOLE word is kept, not just bit 0: `FLAG_MARKERS` below decides
    // whether more bootstrap arguments follow.
    // A dynamic constant's `Integer` is read before the pool (wave 47).
    let alt_flags: i32 = if info.bsm_method == ALT_METAFACTORY {
        let cm = shared.classes.class_manager.read();
        cm.get_class(current_class_id)
            .and_then(|class| {
                lambda_int_arg(&class.constant_pool, &info.bootstrap_arg_indices, condy, 3)
            })
            .unwrap_or(0)
    } else {
        0
    };
    let serializable_flag = (alt_flags & FLAG_SERIALIZABLE) != 0;

    // `FLAG_MARKERS` names ADDITIONAL interfaces the spun proxy implements on
    // top of the functional interface. Real HotSpot puts them in the generated
    // class's `implements` clause, so `marker.isInstance(lambda)` and a
    // `checkcast` to the marker both succeed. CratonVM's proxy is a synthetic
    // ClassId with no ClassStore hierarchy, so the list has to be carried
    // beside it — see `record_lambda_proxy_markers`. Dropping it (which this
    // path used to do) makes every intersection-cast lambda fail its own cast.
    //
    // Each marker is resolved HERE, through the host's defining loader, as
    // the `CONSTANT_Class` static argument it is (HotSpot resolves a
    // bootstrap's static arguments before invoking it), so a later
    // `checkcast` / `instanceof` asks the hierarchy of the class the host
    // named instead of loading a same-named copy through the built-in chain.
    // A name that does not resolve is kept by name only, which is the
    // pre-existing behaviour (the linkage itself does not fail on it).
    let marker_interfaces: Vec<LambdaMarker> = if (alt_flags & FLAG_MARKERS) != 0 {
        read_marker_interfaces(shared, current_class_id, &info.bootstrap_arg_indices, condy)
            .into_iter()
            .map(|name| {
                let class_id = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    &mut *thread,
                    current_class_id,
                    &name,
                )
                .ok()
                .filter(|cid| *cid != ClassId::new(0));
                LambdaMarker { name, class_id }
            })
            .collect()
    } else {
        Vec::new()
    };
    // `FLAG_BRIDGES`: extra descriptors of the SAM name this proxy answers.
    let bridge_descriptors: Vec<Arc<str>> = if (alt_flags & FLAG_BRIDGES) != 0 {
        read_bridge_descriptors(
            shared,
            current_class_id,
            &info.bootstrap_arg_indices,
            alt_flags,
            condy,
        )
    } else {
        Vec::new()    };

    // Allocate a synthetic proxy ClassId
    let proxy_class_id = shared.alloc_lambda_proxy_id();

    // Build the LambdaCallSite
    let call_site = LambdaCallSite {
        functional_interface: Arc::from(functional_interface),
        functional_interface_id,
        sam_method_name: Arc::from(info.target_name.clone()),
        sam_descriptor: Arc::from(sam_erased_desc),
        impl_handle,
        instantiated_descriptor: Arc::from(instantiated_desc),
        capture_types: capture_types.clone(),
        proxy_class_id,
        serializable_flag,
    };

    // Register the lambda proxy and cache the call site
    let registered = {
        let mut proxies = shared.classes.lambda_proxies.write();
        if lambda_proxy_table_has_room(proxies.len()) {
            proxies.insert(proxy_class_id, Arc::new(call_site.clone()));
            true
        } else {
            false
        }
    };
    // Record the defining class (where this invokedynamic lives) so reflection
    // name natives report `<host>$$Lambda/0x<id>` and a HotSpot-correct nest host
    // — even for a cross-class method reference whose impl method is elsewhere.
    // Bounded by the same cap as `lambda_proxies`. bug-06 fam5 #1.
    if registered {
        shared
            .classes
            .lambda_proxy_hosts
            .write()
            .insert(proxy_class_id, current_class_id);
        // gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 6): HotSpot
        // defines a hidden class here and `-Xlog:class+load` prints it
        // (`<host>$$Lambda/0x.. source: <host>`); no class file reaches the
        // class-load hook for this proxy, so it is logged at its
        // registration, after the host is recorded. One logger probe when
        // the tag is off; no guard is held here.
        crate::runtime::unified_logging::log_lambda_proxy_class_load(shared, proxy_class_id);
        record_lambda_proxy_markers_resolved(
            shared.vm_identity,
            proxy_class_id,
            &marker_interfaces,
        );
        record_lambda_proxy_bridges(shared.vm_identity, proxy_class_id, &bridge_descriptors);
        // A live instance must keep the host's (user) loader alive, as the
        // spun class's defining loader does in HotSpot.
        pin_lambda_proxy_to_host_loader(shared.vm_identity, proxy_class_id, current_class_id);
        // The i54-L1 retirement: the tool interfaces' class events of the
        // hidden class HotSpot defines here. gci i12/ev
        // (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 1c): a C agent's `ClassPrepare` / `ClassLoad` of the proxy runs
        // with this thread's JNI binding installed from `thread` itself.
        #[cfg(feature = "experimental-debug")]
        {
            let tid = thread.thread_id.0;
            crate::runtime::interpreter::post_on_own_thread(thread, || {
                crate::debug::lambda_proxy_minted(shared, tid, proxy_class_id)
            });
        }
    }
    // An entry that other instructions of the class share is linked PER
    // INSTRUCTION (JVMS §5.4.3.6): each gets its own proxy class, as HotSpot
    // spins one class per linkage, so `String::length` written twice yields
    // two classes. Its row lives in the per-instruction table, never in the
    // per-entry `resolution_cache` row, which would hand this instruction's
    // proxy to all of them. See `lambda_entry_is_shared`.
    let per_instruction = if lambda_entry_is_shared(shared, current_class_id, cp_index) {
        let key = indy_instruction_key(
            shared,
            &thread.frames[frame_idx],
            current_class_id,
            cp_index,
            singleton_site,
        );
        Some(publish_lambda_instruction_site(
            shared,
            key,
            &info.target_descriptor,
            proxy_class_id,
            capture_types.len(),
        ))
    } else {
        None
    };
    // First writer wins (JVMS §5.4.3.6): threads racing through a cold site
    // each bootstrap, but every one must go on with the ONE installed call
    // site. Publishing last-writer-wins and minting from this thread's own
    // proxy gave each racer a different non-capturing "singleton"
    // (`LambdaIdentityProbe`'s `concurrentFirstUse`). A loser's proxy row
    // stays registered but is never reached again.
    let proxy_class_id = match per_instruction {
        Some(installed) => installed,
        None => {
            let mut cache = shared.classes.resolution_cache.write();
            match cache.get_call_site(current_class_id, cp_index) {
                Some(ResolvedCallSite::Lambda(installed)) => installed.proxy_class_id,
                _ => {
                    // Not installed when a redefinition of the caller may have
                    // replaced the pool this bootstrap read (interpreter round
                    // i1 wave 23, lane L5): this execution goes on with its
                    // own proxy, the next bootstraps the new pool's entry.
                    let _ = cache.put_call_site_as_of(
                        current_class_id,
                        cp_index,
                        ResolvedCallSite::Lambda(call_site),
                        info.fill_as_of,
                    );
                    proxy_class_id
                }
            }
        }
    };

    // Now execute: pop captured values, allocate proxy object, push it
    allocate_lambda_proxy(
        shared,
        thread,
        frame_idx,
        proxy_class_id,
        capture_types.len(),
        singleton_site,
    )
}

/// Read `altMetafactory`'s `FLAG_MARKERS` block out of the bootstrap static
/// arguments: `arg[4]` is `markerCount` (an int), `arg[5 .. 5+markerCount]` are
/// `CONSTANT_Class` entries. Returns the internal names, in declaration order.
///
/// Tolerant by construction: a short or malformed block yields the prefix it
/// could read rather than an error. A bootstrap-method attribute that disagrees
/// with its own flags word is a broken class file, but refusing to link the
/// call site would turn a wrong `instanceof` answer into a hard failure of an
/// otherwise-working lambda.
pub(in crate::runtime::invokedynamic) fn read_marker_interfaces(
    shared: &SharedVm,
    current_class_id: ClassId,
    arg_indices: &[u16],
    condy: &[Option<LambdaCondyArg>],
) -> Vec<Arc<str>> {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(current_class_id) else {
        return Vec::new();
    };
    marker_interfaces_in(&class.constant_pool, arg_indices, condy)
}

/// [`read_marker_interfaces`] over the host's constant pool, a dynamic
/// constant's value read before the pool (wave 47).
pub(in crate::runtime::invokedynamic) fn marker_interfaces_in(
    cp: &ConstantPool,
    arg_indices: &[u16],
    condy: &[Option<LambdaCondyArg>],
) -> Vec<Arc<str>> {
    let count = match lambda_int_arg(cp, arg_indices, condy, 4) {
        Some(n) if n > 0 => n as usize,
        _ => return Vec::new(),
    };
    // `markerCount` is class-file data: sized by the static arguments that can
    // actually follow it, never by the count (a crafted `0x7fffffff` asked for
    // a 32 GiB reservation, which aborts the process).
    let room = arg_indices.len().saturating_sub(5);
    let mut out: Vec<Arc<str>> = Vec::with_capacity(count.min(room));
    for k in 0..count {
        if 5 + k >= arg_indices.len() {
            break;
        }
        if let Some(name) = lambda_class_arg(cp, arg_indices, condy, 5 + k) {
            out.push(name);
        }
    }
    out
}

/// Read `altMetafactory`'s `FLAG_BRIDGES` block out of the bootstrap static
/// arguments: after the flags word (`arg[3]`) and the marker block when
/// `FLAG_MARKERS` is set (`arg[4]` = `markerCount`, then the markers), one int
/// `bridgeCount` and then that many `CONSTANT_MethodType` entries. Returns the
/// descriptors, in declaration order, interned.
///
/// Tolerant like [`read_marker_interfaces`]: a short or malformed block yields
/// the prefix it could read.
pub(in crate::runtime::invokedynamic) fn read_bridge_descriptors(
    shared: &SharedVm,
    current_class_id: ClassId,
    arg_indices: &[u16],
    alt_flags: i32,
    condy: &[Option<LambdaCondyArg>],
) -> Vec<Arc<str>> {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(current_class_id) else {
        return Vec::new();
    };
    bridge_descriptors_in(&class.constant_pool, arg_indices, alt_flags, condy)
}

/// [`read_bridge_descriptors`] over the host's constant pool, a dynamic
/// constant's value read before the pool (wave 47).
pub(in crate::runtime::invokedynamic) fn bridge_descriptors_in(
    cp: &ConstantPool,
    arg_indices: &[u16],
    alt_flags: i32,
    condy: &[Option<LambdaCondyArg>],
) -> Vec<Arc<str>> {
    let int_arg =
        |pos: usize| lambda_int_arg(cp, arg_indices, condy, pos).map(|n| n.max(0) as usize);
    let count_pos = if (alt_flags & FLAG_MARKERS) != 0 {
        match int_arg(4) {
            Some(markers) => 5usize.saturating_add(markers),
            None => return Vec::new(),
        }
    } else {
        4
    };
    let Some(count) = int_arg(count_pos) else {
        return Vec::new();
    };
    let mut out: Vec<Arc<str>> = Vec::with_capacity(count.min(8));
    for k in 0..count {
        let position = count_pos.saturating_add(1 + k);
        if position >= arg_indices.len() {
            break;
        }
        if let Some(desc) = lambda_method_type_arg(cp, arg_indices, condy, position) {
            out.push(cratonvm_types::intern_arc(&desc));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// altMetafactory marker interfaces
//
// A spun lambda proxy implements its functional interface PLUS every interface
// named in `altMetafactory`'s `FLAG_MARKERS` block. CratonVM's proxy classes
// are synthetic ClassIds (>= 0x8000_0000) that are deliberately absent from the
// ClassStore, so they have no `interfaces` vector to append to and
// `LambdaCallSite` (`classloading/src/resolution.rs`) records only the single
// `functional_interface`. The marker list therefore lives in a side table keyed
// by `(vm_identity, proxy_class_id)`, exactly like `LAMBDA_SINGLETON_CACHE`
// above: `alloc_lambda_proxy_id` never recycles an id, and the `vm_identity`
// half keeps two VMs in one test process from aliasing each other.
//
// Entries hold interned names and class ids, never `ObjectRef`s, so unlike the
// singleton cache this table needs no GC root scan or post-compaction remap.
// It is populated only by intersection-cast lambdas, which are rare; the
// `ANY_LAMBDA_MARKERS` gate keeps the (hot) `instanceof`-on-a-lambda path from
// taking the lock at all in the overwhelmingly common empty case.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process has recorded a marker interface.
/// Read before the mutex on every marker query; see the module note above.
pub(in crate::runtime::invokedynamic) static ANY_LAMBDA_MARKERS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One `altMetafactory` marker interface of a lambda proxy.
#[derive(Clone, Debug)]
pub struct LambdaMarker {
    /// The internal name, as the host's constant pool (or the reflective
    /// caller) spells it.
    pub name: Arc<str>,
    /// The class that name resolved to through the HOST's defining loader when
    /// the call site was bootstrapped — the `CONSTANT_Class` static argument
    /// resolution HotSpot performs before it invokes `altMetafactory`. `None`
    /// for the reflective path (its markers arrive as names) and for a name
    /// that did not resolve; those are looked up by name when asked.
    pub class_id: Option<ClassId>,
}

#[allow(clippy::type_complexity)]
pub(in crate::runtime::invokedynamic) static LAMBDA_PROXY_MARKERS: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[LambdaMarker]>>>,
> = std::sync::OnceLock::new();

#[allow(clippy::type_complexity)]
pub(in crate::runtime::invokedynamic) fn lambda_proxy_markers(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[LambdaMarker]>>> {
    LAMBDA_PROXY_MARKERS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record the `altMetafactory` marker interfaces of a freshly-registered lambda
/// proxy, by name only. A no-op for the (usual) empty list.
///
/// `pub` because the REFLECTIVE metafactory path reaches this from outside the
/// `vm` crate: `native-builtins`' `LambdaMetafactory.altMetafactory` shim reads
/// the packed `Object[]` and hands the names back through
/// `NativeContext::register_lambda_proxy_markers`, whose `vm` implementation
/// calls this. The `invokedynamic` path records resolved markers instead
/// ([`record_lambda_proxy_markers_resolved`]).
pub fn record_lambda_proxy_markers(
    vm_identity: usize,
    proxy_class_id: ClassId,
    markers: &[Arc<str>],
) {
    let markers: Vec<LambdaMarker> = markers
        .iter()
        .map(|name| LambdaMarker {
            name: Arc::clone(name),
            class_id: None,
        })
        .collect();
    record_lambda_proxy_markers_resolved(vm_identity, proxy_class_id, &markers);
}

/// Whether a lambda proxy table holding `len` rows (`ClassRealm::lambda_proxies`,
/// the marker and bridge side tables) takes one more.
///
/// Always, since nio21 wave 12 L1 (page
/// `r21-lambda-proxy-registration-cap-drops-the-100001st-proxy-FIXED-20261009.md`):
/// past `MAX_LAMBDA_PROXIES` (100 000) a newly linked lambda used to get a
/// proxy id that was never registered, so its instance answered as an unknown
/// class (`getClass()`, SAM dispatch, marker `instanceof`, reflective name).
/// HotSpot has no such cap: a spun class lives as long as its host's loader.
/// The tables are bounded by the loaded code: every proxy row goes when its
/// host class unloads (`memory::gc` with `forget_unloaded_lambda_proxy_rows`)
/// and with its VM. `CRATONVM_LAMBDA_PROXY_UNCAPPED=0` restores the cap; the
/// switch is read only once a table is AT the cap, so never below it.
pub(crate) fn lambda_proxy_table_has_room(len: usize) -> bool {
    len < crate::vm::MAX_LAMBDA_PROXIES
        || true
}

/// [`record_lambda_proxy_markers`] with each marker's class resolved.
pub fn record_lambda_proxy_markers_resolved(
    vm_identity: usize,
    proxy_class_id: ClassId,
    markers: &[LambdaMarker],
) {
    if markers.is_empty() {
        return;
    }
    let mut table = lambda_proxy_markers().lock();
    // Same rule as `lambda_proxies` itself — a proxy that could not be
    // registered has no marker list to answer for either.
    if !lambda_proxy_table_has_room(table.len()) {
        return;
    }
    table.insert((vm_identity, proxy_class_id), Arc::from(markers.to_vec()));
    drop(table);
    ANY_LAMBDA_MARKERS.store(true, std::sync::atomic::Ordering::Release);
}

/// The `altMetafactory` marker interfaces recorded for `proxy_class_id`, or
/// `None` when it has none (the common case).
pub fn lambda_proxy_marker_interfaces(
    vm_identity: usize,
    proxy_class_id: ClassId,
) -> Option<Arc<[LambdaMarker]>> {
    if !ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    lambda_proxy_markers()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .cloned()
}

// ---------------------------------------------------------------------------
// altMetafactory bridge descriptors
//
// `FLAG_BRIDGES` names extra descriptors of the SAM name that HotSpot's spun
// class implements as forwarding bridges. Same storage shape and reasoning as
// the marker table above: keyed `(vm_identity, proxy_class_id)`, ids never
// recycled, interned strings only (no GC scan), capped like `lambda_proxies`,
// and an `ANY_LAMBDA_BRIDGES` gate so the lambda dispatch path takes no lock
// in the common no-bridge process. Only a descriptor that is NOT the SAM's
// ever asks (see `lambda_accepts_descriptor`), so the gate is off the SAM path.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process has recorded a bridge descriptor.
pub(in crate::runtime::invokedynamic) static ANY_LAMBDA_BRIDGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[allow(clippy::type_complexity)]
pub(in crate::runtime::invokedynamic) static LAMBDA_PROXY_BRIDGES: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[Arc<str>]>>>,
> = std::sync::OnceLock::new();

#[allow(clippy::type_complexity)]
pub(in crate::runtime::invokedynamic) fn lambda_proxy_bridges(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[Arc<str>]>>> {
    LAMBDA_PROXY_BRIDGES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record the `altMetafactory` `FLAG_BRIDGES` descriptors of a freshly
/// registered lambda proxy. A no-op for the (usual) empty list.
///
/// `pub` for the same reason as [`record_lambda_proxy_markers`]: the
/// reflective `altMetafactory` shim hands its bridges over through
/// `NativeContext::register_lambda_proxy_bridges`.
pub fn record_lambda_proxy_bridges(
    vm_identity: usize,
    proxy_class_id: ClassId,
    bridges: &[Arc<str>],
) {
    if bridges.is_empty() {
        return;
    }
    let mut table = lambda_proxy_bridges().lock();
    if !lambda_proxy_table_has_room(table.len()) {
        return;
    }
    table.insert((vm_identity, proxy_class_id), Arc::from(bridges.to_vec()));
    drop(table);
    ANY_LAMBDA_BRIDGES.store(true, std::sync::atomic::Ordering::Release);
}

/// Is `descriptor` one of the `FLAG_BRIDGES` descriptors recorded for
/// `proxy_class_id`? One atomic load when no bridge was ever recorded.
pub fn lambda_proxy_has_bridge(
    vm_identity: usize,
    proxy_class_id: ClassId,
    descriptor: &str,
) -> bool {
    if !ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        return false;
    }
    lambda_proxy_bridges()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .is_some_and(|bridges| bridges.iter().any(|b| &**b == descriptor))
}

/// The `FLAG_BRIDGES` descriptors recorded for `proxy_class_id`, or `None`
/// when it has none (the common case; one atomic load when no bridge was ever
/// recorded). Read by reflection (`getDeclaredMethods()` lists one method per
/// bridge, as HotSpot's spun class declares them) and by lambda serialization,
/// through `NativeContext::lambda_proxy_serial_metadata`.
pub fn lambda_proxy_bridge_descriptors(
    vm_identity: usize,
    proxy_class_id: ClassId,
) -> Option<Arc<[Arc<str>]>> {
    if !ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    lambda_proxy_bridges()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .cloned()
}

// ---------------------------------------------------------------------------
// Lambda proxy -> host loader liveness
//
// HotSpot defines a lambda's spun class in its host's loader, so a live lambda
// instance keeps that loader (and the host, and the implementation method)
// alive. CratonVM's proxy is a synthetic class id with no defining-loader row,
// so the collector's instance->loader edge (`cratonvm_types::loader_pin`) is
// given one here: every proxy whose host a USER loader defined gets a pin row
// naming the host's loader. `bootstrap_lambda` writes it (so the first
// collection already honours it), and `repin_lambda_proxy_loaders` re-adds the
// rows after every collection, because the post-GC reconciliation
// (`classloader::gc_reconcile_defining_loaders`) REPLACES the VM's whole pin
// set from the defining-loader table, which has no proxy rows.
//
// Keyed per VM; only user-loader hosts are listed, so a program without user
// loaders never takes the lock (`ANY_USER_LOADER_LAMBDA`). Rows go with their
// host (`forget_unloaded_lambda_proxy_rows`) and with the VM.
// See docs/feature-designs/real-jdk-mode.md.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process was spun by a user-loader host.
pub(in crate::runtime::invokedynamic) static ANY_USER_LOADER_LAMBDA: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `vm_identity` -> `(proxy, host)` for every proxy whose host a user loader
/// defined.
pub(in crate::runtime::invokedynamic) type UserLoaderLambdaMap = rustc_hash::FxHashMap<usize, Vec<(ClassId, ClassId)>>;

pub(in crate::runtime::invokedynamic) static USER_LOADER_LAMBDAS: std::sync::OnceLock<parking_lot::Mutex<UserLoaderLambdaMap>> =
    std::sync::OnceLock::new();

pub(in crate::runtime::invokedynamic) fn user_loader_lambdas() -> &'static parking_lot::Mutex<UserLoaderLambdaMap> {
    USER_LOADER_LAMBDAS.get_or_init(|| parking_lot::Mutex::new(UserLoaderLambdaMap::default()))
}

/// Give a freshly registered proxy the loader-pin row of its host, when a user
/// loader defined the host. A no-op (one registry probe) for a built-in host.
pub(crate) fn pin_lambda_proxy_to_host_loader(
    vm_identity: usize,
    proxy_class_id: ClassId,
    host_class_id: ClassId,
) {
    let Some(loader_addr) =
        cratonvm_types::loader_pin::loader_pin_addr_for_vm(vm_identity, host_class_id.as_u32())
    else {
        return;
    };
    user_loader_lambdas()
        .lock()
        .entry(vm_identity)
        .or_default()
        .push((proxy_class_id, host_class_id));
    ANY_USER_LOADER_LAMBDA.store(true, std::sync::atomic::Ordering::Release);
    cratonvm_types::loader_pin::set_loader_pin(vm_identity, proxy_class_id.as_u32(), loader_addr);
}

/// Record the host of a proxy the REFLECTIVE `LambdaMetafactory` path
/// registered (`NativeContext::register_lambda_proxy_host`): the `Lookup`'s
/// lookup class, which is HotSpot's host (`caller.lookupClass()`). Writes the
/// same two rows `bootstrap_lambda` writes for an `invokedynamic` proxy — the
/// `lambda_proxy_hosts` row (name, nest host, loader-faithful impl and marker
/// resolution) and the loader pin — so the proxy also unloads with its host
/// (`forget_unloaded_lambda_proxy_rows`, which drops the reflective
/// `CallSite` cache rows keyed by that host in the same pass).
///
/// Refused (nothing recorded, the pre-existing impl-owner fallback stays):
/// proxy id `0` (registration declined), a proxy this VM did not register, a
/// host that is not a loaded class of this VM, and a proxy that already has a
/// host (a proxy is spun once).
pub(crate) fn record_reflective_lambda_proxy_host(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    host_class_id: ClassId,
) -> bool {
    // One lock at a time: no guard below is held while another is taken.
    if proxy_class_id.as_u32() == 0 {
        return false;
    }
    let registered = shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&proxy_class_id);
    if !registered {
        return false;
    }
    let host_loaded = shared
        .classes
        .class_manager
        .read()
        .get_class(host_class_id)
        .is_some();
    if !host_loaded {
        return false;
    }
    {
        let mut hosts = shared.classes.lambda_proxy_hosts.write();
        if hosts.contains_key(&proxy_class_id) {
            return false;
        }
        hosts.insert(proxy_class_id, host_class_id);
    }
    pin_lambda_proxy_to_host_loader(shared.vm_identity, proxy_class_id, host_class_id);
    true
}

/// Re-add the proxy -> host-loader pin rows of `vm_identity` after the post-GC
/// reconciliation rebuilt that VM's pin set. Call right after
/// `classloader::gc_reconcile_defining_loaders` returns: a host whose loader
/// died this cycle has no row any more, so its proxies get none either (and
/// `forget_unloaded_lambda_proxy_rows` drops them in the same pause).
pub fn repin_lambda_proxy_loaders(vm_identity: usize) {
    if !ANY_USER_LOADER_LAMBDA.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let table = user_loader_lambdas().lock();
    let Some(rows) = table.get(&vm_identity) else {
        return;
    };
    for &(proxy, host) in rows {
        if let Some(addr) =
            cratonvm_types::loader_pin::loader_pin_addr_for_vm(vm_identity, host.as_u32())
        {
            cratonvm_types::loader_pin::set_loader_pin(vm_identity, proxy.as_u32(), addr);
        }
    }
}

/// Does lambda proxy `proxy_class_id` satisfy `target` by way of one of its
/// `altMetafactory` marker interfaces?
///
/// A marker satisfies the target when it IS the target or extends it — a marker
/// of `java/util/List` also answers `instanceof Collection`, because HotSpot put
/// `List` in the spun class's `implements` clause and the ordinary interface
/// hierarchy takes it from there. Called from
/// `runtime::interpreter::typecheck::lambda_proxy_satisfies`, the single choke
/// point for `checkcast` / `instanceof` / `Class.isInstance` /
/// `Class.isAssignableFrom` on a lambda proxy.
pub fn lambda_proxy_marker_satisfies(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    target_class_id: ClassId,
    target_name: &str,
) -> bool {
    let Some(markers) = lambda_proxy_marker_interfaces(shared.vm_identity, proxy_class_id) else {
        return false;
    };
    // The marker names were written in the HOST class's constant pool, so they
    // denote classes as the host's defining loader sees them. The
    // `invokedynamic` path resolved them through that loader at bootstrap
    // (`LambdaMarker::class_id`), so the common case asks the class hierarchy
    // and loads nothing here — a `checkcast` must not reach a safepoint on
    // this path for a class its bootstrap already resolved. For a marker with
    // no recorded class (the reflective metafactory path resolves none -- its
    // host is the lookup class -- or the name did not resolve), an
    // already-loaded copy visible to the host's
    // loader is asked first, and the built-in chain is the last resort.
    // Resolving through the built-in chain alone (`load_class_concurrent`, the
    // only lookup this used to make) picked an arbitrary copy when a webapp or
    // forked test loader defines its own.
    let host_loader = || {
        let host = shared
            .classes
            .lambda_proxy_hosts
            .read()
            .get(&proxy_class_id)
            .copied()?;
        shared.classes.class_manager.read().get_loader_id(host)
    };
    // JIT r25 wave 6 (W6-R fix, probe `R25W6ProxyWaysLoaders`): under
    // `NamespacedClassTypes` (on by default, and under `--real-jdk`) a marker
    // the bootstrap resolved is a CLASS, not a name: another loader's
    // same-named interface is a different type, as HotSpot answers. The name
    // match stays for a marker with no recorded class (the reflective path).
    let namespaced = shared
        .classes
        .aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes);
    for marker in markers.iter() {
        if &*marker.name == target_name && !(namespaced && marker.class_id.is_some()) {
            return true;
        }
        if marker.class_id == Some(target_class_id) {
            return true;
        }
        let resolved = match marker.class_id {
            Some(id) => Ok(id),
            None => {
                let loader = host_loader();
                let loader_visible = loader.and_then(|loader| {
                    shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id_for_requester(&marker.name, loader)
                });
                match loader_visible {
                    Some(id) => Ok(id),
                    None => shared.load_class_concurrent(&marker.name),
                }
            }
        };
        if let Ok(marker_id) = resolved {
            if shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(marker_id, target_class_id)
            {
                return true;
            }
        }
    }
    false
}

/// Execute a cached lambda call site: pop captures, allocate proxy, push result.
pub(in crate::runtime::invokedynamic) fn execute_cached_lambda(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    lcs: &LambdaCallSite,
) -> Result<(), MethodCallFailed> {
    allocate_lambda_proxy(
        shared,
        thread,
        frame_idx,
        lcs.proxy_class_id,
        lcs.capture_types.len(),
        None,
    )
}

// Zero-capture lambda singleton cache — mirrors real HotSpot's
// `InnerClassLambdaMetafactory` behaviour of caching a single `INSTANCE`
// per spun lambda class when the lambda captures nothing. Some code
// (Spring AOT's bean-override identity check across an original context
// and its AOT-replayed context) depends on `==`/`.equals()` identity
// holding for two separate invocations of the same non-capturing lambda
// expression. `proxy_class_id` is a monotonically-increasing synthetic id
// (`SharedVm::alloc_lambda_proxy_id`) that is never recycled, so unlike
// the real (recyclable) `ClassId` space used for loaded classes, keying
// this cache directly by `(vm_identity, proxy_class_id)` carries no
// aliasing risk. GC-scanned/remapped the same way as the
// `Integer.valueOf` cache in `native-builtins/src/lang_math.rs` (see
// `gc_scan_lambda_singleton_roots` / `gc_update_lambda_singleton_refs`,
// wired into `vm/src/memory/{roots.rs,gc.rs}`).
///
/// The key carries the invokedynamic INSTRUCTION's bci as well as the proxy
/// class id. JVMS 5.4.3.6 links each `invokedynamic` *instruction* to its own
/// call site, even when javac folds several occurrences onto ONE
/// `CONSTANT_InvokeDynamic` entry -- which it does for repeated method
/// references (`Foo::bar` written twice compiles to two instructions sharing a
/// CP index). HotSpot therefore hands out a DISTINCT instance per occurrence,
/// while keying only by `proxy_class_id` (which is per CP entry) collapsed them
/// into one. Spring's `WebClient.Builder.defaultStatusHandler` keys a
/// `LinkedHashMap` on the predicate, so two `HttpStatusCode::is4xxClientError`
/// registrations became ONE entry and the second handler silently replaced the
/// first (`web.reactive.function.client.DefaultWebClientTests
/// .onStatusHandlerRegisteredGlobally`).
///
/// The bci alone does not name an instruction, though: two METHODS of one
/// class that each contain the shared entry at the same bci (two
/// `return String::length;` bodies, both at bci 0) collided on it. The key
/// therefore also carries a hash of the executing method's name and
/// descriptor (`lambda_site_method_key`).
///
/// Both tiers use ONE key per instruction: the interpreter derives it from the
/// executing frame (method, pc after the instruction), and the compiled bridge
/// carries the same value, computed at compile time by
/// [`lambda_singleton_site_for`]. Until 2026-09-24 the bridge keyed every site
/// on its synthetic frame instead, so a non-capturing lambda changed identity
/// when its method compiled.
///
/// An `RwLock` over an `FxHashMap`, not a `Mutex` over a SipHash map: the
/// lookup below runs on the first evaluation of a non-capturing lambda
/// expression on each thread, and it only ever reads once the site is warm.
/// Writers are the one-time mint per site and the GC remap.
///
/// Each row's value is a [`LambdaSingletonSlot`], shared by `Arc` with the
/// warm-path memos (round i1 wave 20, lane L4): a thread's per-entry indy row
/// (`CachedIndySite::Lambda::singleton`) and a compiled bridge site
/// (`JitIndyGenericSite::singleton_slot`) keep the slot and read the instance
/// from it with one load — no lock, no hash, no refcount. The slot stays the
/// only copy of the reference: the `lambda-singletons` root row scans it and
/// the remap rewrites it through this map, and a row that leaves the map
/// retires its slot first (0), so a memo that outlives its row falls back to
/// the map instead of reading an unscanned reference.
pub(in crate::runtime::invokedynamic) static LAMBDA_SINGLETON_CACHE: std::sync::OnceLock<parking_lot::RwLock<LambdaSingletonMap>> =
    std::sync::OnceLock::new();

/// `(vm_identity, proxy_class_id, method key, invokedynamic pc)` -> the site's singleton.
pub(in crate::runtime::invokedynamic) type LambdaSingletonMap =
    rustc_hash::FxHashMap<(usize, ClassId, u64, usize), Arc<LambdaSingletonSlot>>;

pub(in crate::runtime::invokedynamic) fn lambda_singleton_cache() -> &'static parking_lot::RwLock<LambdaSingletonMap> {
    LAMBDA_SINGLETON_CACHE.get_or_init(|| parking_lot::RwLock::new(LambdaSingletonMap::default()))
}

/// One zero-capture lambda singleton: the instance's address, or 0 once its
/// [`LAMBDA_SINGLETON_CACHE`] row was dropped (host unloaded, VM disposed).
///
/// Written only under the map's write lock (the mint, the GC remap, the
/// retire); read lock-free by the warm-path memos. A reader never holds the
/// value across a safepoint — it pushes it, or returns it to compiled code
/// through `native_pending_return` — exactly as it did the map's value.
///
/// `#[repr(transparent)]` (nio21 wave 10, lane L3): compiled code reads the
/// word at the slot's own address (`JitIndyGenericSite::constant_word`), with
/// the same no-safepoint-between-load-and-use rule.
#[repr(transparent)]
pub struct LambdaSingletonSlot(pub(in crate::runtime::invokedynamic) std::sync::atomic::AtomicUsize);

impl LambdaSingletonSlot {
    pub(in crate::runtime::invokedynamic) fn new(obj: ObjectRef) -> Self {
        Self(std::sync::atomic::AtomicUsize::new(obj.as_ptr() as usize))
    }

    /// The instance, or `None` for a retired slot.
    #[inline]
    pub(in crate::runtime::invokedynamic) fn get(&self) -> Option<ObjectRef> {
        let addr = self.0.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: a non-zero value is the address of the singleton the mint
        // stored, kept current by the GC remap (`gc_update_lambda_singleton_refs`).
        (addr != 0).then(|| unsafe { ObjectRef::from_raw(addr as *mut u8) })
    }

    pub(in crate::runtime::invokedynamic) fn set(&self, obj: ObjectRef) {
        self.0
            .store(obj.as_ptr() as usize, std::sync::atomic::Ordering::Release);
    }

    /// The row is leaving the map: memos that still hold the slot must stop
    /// answering from it.
    pub(in crate::runtime::invokedynamic) fn retire(&self) {
        self.0.store(0, std::sync::atomic::Ordering::Release);
    }
}

/// The slot of the singleton row `(this VM, proxy, site)`, for a warm-path
/// memo to keep. Takes the map's read lock: called once per memo fill, never
/// per evaluation.
pub(in crate::runtime::invokedynamic) fn lambda_singleton_slot(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    site: (u64, usize),
) -> Option<Arc<LambdaSingletonSlot>> {
    lambda_singleton_cache()
        .read()
        .get(&(shared.vm_identity, proxy_class_id, site.0, site.1))
        .cloned()
}

/// GC root scan hook (`memory::native_roots`, the `lambda-singletons` row).
/// Visits each cached zero-capture singleton of the VM with its PROXY class
/// id, which the row hands to `defer_or_root` as the owner.
///
/// Loader-conditional since wave 8, like the generic indy call sites
/// ([`gc_scan_generic_indy_roots`]): a proxy spun by a user-loader host has a
/// `loader_pin` row naming that loader (`pin_lambda_proxy_to_host_loader`), so
/// in a licensed cycle its singleton is pinned to the loader instead of rooted.
/// It has to be: the singleton's own pin row keeps the loader alive, so rooting
/// it outright would keep every user loader that ever evaluated a
/// non-capturing lambda alive for the life of the VM. A proxy of a built-in
/// host has no row and is rooted, as before.
pub fn gc_scan_lambda_singleton_roots(vm_identity: usize, visit: &mut dyn FnMut(u32, ObjectRef)) {
    let cache = lambda_singleton_cache().read();
    for (&(vid, proxy, _, _), slot) in cache.iter() {
        if vid == vm_identity {
            if let Some(obj_ref) = slot.get() {
                visit(proxy.as_u32(), obj_ref);
            }
        }
    }
}

/// GC post-compaction hook — called from `vm/src/memory/gc.rs`. Remaps
/// every cached singleton for the active VM through the GC's pointer map.
pub fn gc_update_lambda_singleton_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    // The write lock, as before the slots existed: it excludes a concurrent
    // mint, which is the only other writer of a live slot.
    let cache = lambda_singleton_cache().write();
    for (&(vid, _, _, _), slot) in cache.iter() {
        if vid != vm_identity {
            continue;
        }
        let Some(obj_ref) = slot.get() else {
            continue;
        };
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            slot.set(unsafe { ObjectRef::from_raw(new_addr as *mut u8) });
        }
    }
}

/// Pop captured values from the stack, allocate a lambda proxy object, push it.
///
/// `singleton_site`: see [`execute_invokedynamic_at`]; `None` derives the key
/// from the frame.
pub(in crate::runtime::invokedynamic) fn allocate_lambda_proxy(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    proxy_class_id: ClassId,
    num_captures: usize,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    // The singleton key names the INSTRUCTION: method and pc. The pc alone
    // did not -- two methods that each `return String::length;` share one
    // `CONSTANT_InvokeDynamic` entry (so one `proxy_class_id`) AND the same pc,
    // and got one instance where HotSpot links two call sites. Only needed for
    // the zero-capture case, which is the only one that consults the key.
    // The interpreter advanced `pc` past the 5-byte instruction before
    // dispatching it, which is the convention `lambda_singleton_site_for`
    // writes down for the compiled side.
    let site = match singleton_site {
        Some(site) if num_captures == 0 => site,
        // A capturing site never reads the key; the pc stays for the
        // `[WATCH-ALLOC]` diagnostic.
        Some((_, pc)) => (0, pc),
        None if num_captures == 0 => {
            let frame = &thread.frames[frame_idx];
            (lambda_site_method_key(frame), frame.pc)
        }
        None => (0, thread.frames[frame_idx].pc),
    };

    // Pop captured values (pushed left-to-right, pop right-to-left). Inline
    // storage (wave 16): a heap `Vec` here was one allocation per capturing
    // lambda evaluation.
    let mut captures: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(num_captures);
    for _ in 0..num_captures {
        captures.push(thread.frames[frame_idx].stack.pop()?);
    }
    captures.reverse();

    let proxy_ref =
        allocate_lambda_proxy_from_values(shared, thread, proxy_class_id, &mut captures, site)?;
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(proxy_ref)))?;
    Ok(())
}

/// The singleton cache's method component for the frame executing an
/// `invokedynamic`: an Fx hash of its name and descriptor. The class is already
/// in the key (a `proxy_class_id` belongs to one class's constant pool).
pub(in crate::runtime::invokedynamic) fn lambda_site_method_key(frame: &Frame) -> u64 {
    lambda_site_method_key_of(frame.method_name(), frame.method_descriptor())
}

/// [`lambda_site_method_key`] from the name and descriptor directly.
pub(in crate::runtime::invokedynamic) fn lambda_site_method_key_of(method_name: &str, method_descriptor: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    method_name.hash(&mut h);
    method_descriptor.hash(&mut h);
    h.finish()
}

/// Length of the `invokedynamic` instruction (opcode, u16 index, two zeros).
pub(in crate::runtime::invokedynamic) const INVOKEDYNAMIC_LEN: usize = 5;

/// The zero-capture singleton key component of the `invokedynamic` at
/// `indy_bci` in method `method_name``method_descriptor` — the value the
/// interpreter derives from the executing frame, computed without one.
///
/// The pc half is the pc AFTER the instruction (`indy_bci + 5`), because that
/// is what the interpreter's frame holds when it dispatches the instruction
/// (both the decoded and the raw loop advance `pc` first). Compile doors call
/// this so a compiled site keys `LAMBDA_SINGLETON_CACHE` exactly as the
/// interpreted instruction does; see [`make_jit_indy_bridge_site_from_parts`].
pub fn lambda_singleton_site_for(
    method_name: &str,
    method_descriptor: &str,
    indy_bci: usize,
) -> (u64, usize) {
    (
        lambda_site_method_key_of(method_name, method_descriptor),
        indy_bci + INVOKEDYNAMIC_LEN,
    )
}

/// The method name of the synthetic frame `execute_jit_indy_generic_raw`
/// pushes. It names no instruction: the frame path passes the site's own
/// singleton key instead (`execute_invokedynamic_at`).
pub(in crate::runtime::invokedynamic) const JIT_INDY_FRAME_METHOD: &str = "bridge";

/// The frame-free half of [`allocate_lambda_proxy`]: everything from the
/// zero-capture singleton probe to the field stores, given the captures as
/// values rather than as operand-stack entries.
///
/// Split out for the compiled `invokedynamic` bridge, which has the captures in
/// a spill buffer and had to build (and then discard) a whole interpreter frame
/// just to hand them to this code — see `execute_jit_indy_generic_raw`'s fast
/// path. `site` is the singleton cache's per-INSTRUCTION key component,
/// `(method key, pc after the instruction)`. The bridge passes the key its
/// compile door computed for the instruction
/// (`JitIndyGenericSite::singleton_site`, [`lambda_singleton_site_for`]), so a
/// compiled site and the interpreted instruction share one instance.
/// gcf f4/o: `CRATONVM_LAMBDA_PROXY_TLAB` (default ON since the gcg triage, tg1:
/// 4 threads 60 s -> 10 s; `=0` restores the shared path; `runtime_flag_default_on`,
/// read once per VM into `AllocLadderState::lambda_proxy_tlab`) -- capturing
/// lambda proxies try the thread's TLAB before the shared path
/// ([`allocate_lambda_proxy_from_values`]).
pub(in crate::runtime::invokedynamic) fn lambda_proxy_tlab_enabled(shared: &SharedVm) -> bool {
    *shared.mem.alloc_ladder.lambda_proxy_tlab.get_or_init(|| {
        true
    })
}


/// nio21 wave 14, lane L1 (W11L3-2): a capturing proxy allocated in the
/// thread's TLAB, in the legacy shape [`allocate_lambda_proxy_from_values`]'s
/// own TLAB attempt uses, with its captures stored -- or `None` (a TLAB miss,
/// the TLAB switch off, a diagnostic that needs the full path), and the caller
/// takes that full path.
///
/// No capture pins: `tlab_alloc_object` (no guarded refill) never collects,
/// so no capture can move between the caller's decode and the stores -- the
/// fact the full path's own TLAB attempt states. Each call is a fresh object,
/// as HotSpot's capturing lambda is.
pub(in crate::runtime::invokedynamic) fn allocate_lambda_proxy_tlab_fast(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy_class_id: ClassId,
    captures: &[Value],
) -> Option<ObjectRef> {
    if cfg!(feature = "experimental-debug")
        || captures.is_empty()
        || !lambda_proxy_tlab_enabled(shared)
        || cratonvm_types::flags().gc.dbg_deadref_store
    {
        return None;
    }
    let num_captures = captures.len();
    let legacy_total = cratonvm_gc::heap::HEADER_SIZE
        .saturating_add(num_captures.saturating_mul(cratonvm_gc::heap::SLOT_SIZE));
    if legacy_total > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let proxy = crate::runtime::interpreter::tlab_alloc_object(
        thread,
        shared,
        proxy_class_id,
        num_captures,
        legacy_total,
    )?;
    for (i, val) in captures.iter().enumerate() {
        shared.mem.heap.set_field(proxy, i, *val);
    }
    Some(proxy)
}

pub(in crate::runtime::invokedynamic) fn allocate_lambda_proxy_from_values(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy_class_id: ClassId,
    captures: &mut [Value],
    site: (u64, usize),
) -> Result<ObjectRef, MethodCallFailed> {
    let num_captures = captures.len();
    let (site_method, site_pc) = site;

    // Fast path: a zero-capture call site whose singleton was already
    // minted on a prior invocation just returns the cached instance —
    // matches real HotSpot's cached-INSTANCE-field optimization for
    // non-capturing lambdas (see LAMBDA_SINGLETON_CACHE above).
    // Per-INSTRUCTION identity: see LAMBDA_SINGLETON_CACHE's doc comment.
    if num_captures == 0 {
        if let Some(cached) = lambda_singleton_cache()
            .read()
            .get(&(shared.vm_identity, proxy_class_id, site_method, site_pc))
            .and_then(|slot| slot.get())
        {
            // `CRATONVM_DBG_DEADREF_STORE`: is what the cache hands back still
            // a live object?
            //
            // This table is a GC root source and a remap target
            // (`gc_scan_lambda_singleton_roots` / `gc_update_lambda_singleton_refs`,
            // wired in `memory/native_roots.rs`), so a stale entry here means
            // one of those two halves did not run for a collection that moved
            // the instance — which no other instrument in the tree can see,
            // because the value is in neither a frame slot nor a heap field.
            if cratonvm_types::flags().gc.dbg_deadref_store {
                if let Some(reason) = shared
                    .mem
                    .heap
                    .dead_young_ref_reason(cached.as_ptr() as usize)
                {
                    static N: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 8 {
                        eprintln!(
                            "[deadref-singleton] #{n} {reason} the zero-capture lambda \
                             singleton cache returned 0x{:x} for cid={:#x} site_pc={site_pc} — \
                             the cached instance names no live object, so the cache was not \
                             remapped for some collection that moved it.",
                            cached.as_ptr() as usize,
                            proxy_class_id.as_u32(),
                        );
                    }
                }
            }
            return Ok(cached);
        }
    }

    // GC-safety: captures sits in a raw Rust Vec, invisible to the
    // collector (native stale-local family). The fallback allocation path
    // below can force a GC on young-gen exhaustion (maybe_gc_forced_pub),
    // which may relocate any captured object reference. Pin every captured
    // object into native_pin_roots (which the GC remaps) before
    // attempting allocation, and refresh captures from the pins
    // afterward so a relocated capture is never written stale into the new
    // proxy's fields.
    //
    // Concrete failure without this: a bound interface-method-reference
    // lambda (e.g. values::get over TDigestDoubleArray) captured under
    // memory pressure got a stale pre-GC ObjectRef written into its proxy
    // field. Every later dispatch through that proxy read the captured
    // field, found the (now-vacated) old address, and resolved a garbage/
    // zeroed class id there instead of the real receiver class.
    // `CRATONVM_DBG_DEADREF_STORE`: is a capture ALREADY dead before the pin?
    //
    // The pin below protects a capture from the allocation that follows; it
    // cannot resurrect one that was dead when it reached the operand stack.
    // Those two produce the same end state — a proxy field naming reclaimed
    // memory — and the heap-side trap in `GenerationalHeap::set_field` reports
    // the STORE either way, which names this function and stops. Checking here,
    // with the Java frames still in hand, says which of the two it is and where
    // the value came from.
    if cratonvm_types::flags().gc.dbg_deadref_store {
        for (i, c) in captures.iter().enumerate() {
            let Value::Object(Some(o)) = c else { continue };
            let Some(reason) = shared.mem.heap.dead_young_ref_reason(o.as_ptr() as usize) else {
                continue;
            };
            // WHICH FRAMES ALREADY HOLD IT.
            //
            // The chain above says where the value is being consumed; it does
            // not say where it entered. A dead value that appears only in the
            // innermost frames was manufactured during argument passing; one
            // that the OUTERMOST frame also holds arrived from that frame's own
            // code and the search continues above it. Printing the slot on
            // every frame turns that from a guess into a reading, and it is one
            // pass over a stack that is about to fail anyway.
            let dead = o.as_ptr() as usize;
            let stack: Vec<String> = thread
                .frames
                .iter()
                .rev()
                .take(10)
                .map(|f| {
                    let mut holds: Vec<String> = Vec::new();
                    for li in 0..f.locals_len() {
                        if let Value::Object(Some(v)) = f.get_local(li as u16) {
                            if v.as_ptr() as usize == dead {
                                holds.push(format!("local[{li}]"));
                            }
                        }
                    }
                    for si in 0..f.stack.len() {
                        if let Value::Object(Some(v)) = f.stack.get_value(si) {
                            if v.as_ptr() as usize == dead {
                                holds.push(format!("stack[{si}]"));
                            }
                        }
                    }
                    format!(
                        "    at {}.{} pc={}{}",
                        f.class_name(),
                        f.method_name(),
                        f.pc,
                        if holds.is_empty() {
                            String::new()                        } else {
                            format!("   <-- HOLDS IT in {}", holds.join(", "))                        },
                    )
                })
                .collect();
            eprintln!(
                "[deadref-capture] {reason} capture[{i}] = 0x{:x} was ALREADY dead when the \
                 lambda proxy popped it off the operand stack (cid={:#x}, {} captures) — the \
                 pin below cannot help, the value was wrong before this call. \
                 moved_away_to={:?} (needs CRATONVM_DBG_VACATED_FRAMES; Some means the \
                 referent was RELOCATED and a rewrite was missed, None means it was never \
                 relocated — reclaimed while referenced, or never a valid reference)
in_native_pins={} frames={}
{}",
                o.as_ptr() as usize,
                proxy_class_id.as_u32(),
                num_captures,
                cratonvm_gc::gc_quiescence::moved_away_to(o.as_ptr() as usize),
                thread
                    .native_pin_roots
                    .iter()
                    .any(|r| r.as_ptr() as usize == dead),
                thread.frames.len(),
                stack.join(
                    "
"
                ),
            );
        }
    }

    let pin_base = thread.native_pin_roots.len();
    // Inline storage (wave 16): this runs for every capturing lambda
    // evaluation, and a heap `Vec` here was one allocation per evaluation.
    let mut handles: smallvec::SmallVec<[Option<usize>; 8]> =
        smallvec::SmallVec::with_capacity(captures.len());
    for c in captures.iter() {
        if let Value::Object(Some(o)) = c {
            handles.push(Some(thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        } else {
            handles.push(None);
        }
    }

    // Allocate a proxy object on the heap with fields for captured values,
    // through `new`'s shared-path front end (`alloc_object_shared`): the
    // young attempt, then the one escalation every interpreter ladder shares
    // (`collect_and_retry`: publish and retire the TLAB, forced collection,
    // GC-overhead limit, SoftReference clearing, old-gen-spilling retry, G1's
    // last-ditch cycle), the heap dump on OOM, and the allocation counters.
    //
    // gc-common w9-c: this used to be a private copy of the pre-w2-d ladder --
    // one forced collection and a YOUNG-only retry. So a capturing lambda on a
    // heap whose old generation still had room threw `OutOfMemoryError` after
    // a young collection left eden fragmented; softly reachable objects were
    // NOT cleared before that `OutOfMemoryError` (the `SoftReference`
    // guarantee `alloc_object_shared` keeps); the GC-overhead limit never
    // applied; `-XX:+HeapDumpOnOutOfMemoryError` never dumped; and every
    // proxy's bytes were missing from `bytes_allocated_total` and
    // `ThreadMXBean.getThreadAllocatedBytes` (the shared path is not a TLAB,
    // and only `alloc_object_shared` charges it).
    //
    // gcf f4/o (`common-w9c-proposal-lambda-proxy-tlab-allocation`), opt-in
    // `CRATONVM_LAMBDA_PROXY_TLAB=1`: first the thread's TLAB, as `new` does,
    // in the LEGACY shape (`HEADER_SIZE + n * SLOT_SIZE`, which
    // `shape_of_reserved` reads back as legacy) -- the shape every proxy
    // field reader already handles, whatever the shared path would plan for
    // the synthetic class id. A TLAB miss (or a proxy too big for one) takes
    // the shared path below unchanged. No collection can run in the TLAB
    // attempt (`tlab_alloc_object` without the guarded refill never
    // collects), so the pinned captures stay valid either way.
    let tlab_proxy = if lambda_proxy_tlab_enabled(shared) {
        let legacy_total = cratonvm_gc::heap::HEADER_SIZE
            .saturating_add(num_captures.saturating_mul(cratonvm_gc::heap::SLOT_SIZE));
        if legacy_total <= cratonvm_gc::tlab::tlab_max_alloc() {
            crate::runtime::interpreter::tlab_alloc_object(
                thread,
                shared,
                proxy_class_id,
                num_captures,
                legacy_total,
            )
        } else {
            None
        }
    } else {
        None
    };
    let proxy_ref = match tlab_proxy {
        Some(obj) => obj,
        None => match crate::runtime::interpreter::alloc_object_shared(
            shared,
            thread,
            proxy_class_id,
            num_captures,
        ) {
            Ok(obj) => obj,
            Err(e) => {
                thread.native_pin_roots.truncate(pin_base);
                return Err(e);
            }
        },
    };
    // DBG (`CRATONVM_DBG_WATCH_ALLOC_CID=<hex class id>`): arm the young
    // marker's edge trace on THIS proxy the moment it exists.
    //
    // A lambda proxy has no class NAME to point `CRATONVM_DBG_MARK_WHY_CLASS`
    // at — its class id is synthetic (>= 0x8000_0000) and deliberately absent
    // from the ClassStore — so the one instrument that can say "why was this
    // object marked, or why was it not" had nothing to key on. The ids are
    // handed out sequentially and never recycled, so they ARE stable across
    // runs of the same workload, which makes them a usable handle.
    //
    // Added chasing `io.netty.util.internal.ObjectCleanerTest`, where a
    // one-capture proxy (`Comparator.comparing(fn)`'s result) is freed by the
    // first young sweep while nothing in the heap, the roots, or any scanned
    // side table refers to it, and the address is then written into a
    // `Collections$ReverseComparator2` built afterwards.
    // Memoised: this sits on the lambda-proxy allocation path, and an env
    // lookup per allocation is not a diagnostic cost, it is a permanent one.
    // `None` (the overwhelmingly common case) is one `OnceLock` load.
    {
        static WANT_CID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
        let want_cid = *WANT_CID.get_or_init(|| {
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .ok()
                .and_then(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok())
        });
        if let Some(want_cid) = want_cid {
            if proxy_class_id.as_u32() == want_cid {
                let addr = proxy_ref.as_ptr() as usize;
                let mut stk = String::new();
                {
                    use std::fmt::Write as _;
                    for f in thread.frames.iter().rev().take(6) {
                        let _ = write!(
                            stk,
                            "
[WATCH-ALLOC]     at {}.{}",
                            f.class_name(),
                            f.method_name()
                        );
                    }
                }
                eprintln!(
                    "[WATCH-ALLOC] lambda proxy cid={:#x} captures={num_captures} \
                     site_pc={site_pc} -> {addr:#x} tid={} frames={}{}",
                    proxy_class_id.as_u32(),
                    thread.thread_id.0,
                    thread.frames.len(),
                    stk
                );
                // BOTH watches: `set_young_mark_watch` is what
                // `mark_edge_precise` / `mark_young` consult to print
                // `[MARKWHY] young marker reached <addr> via <edge>` — and its
                // SILENCE is the finding when the object is swept.
                // `set_dynamic_watch` additionally reports writes through the
                // cell, which names whoever stores the address afterwards.
                cratonvm_gc::heap::set_young_mark_watch(addr);
                cratonvm_gc::heap::set_dynamic_watch(addr);
            }
        }
    }
    // Refresh any captured object references from their pins — the retry
    // path above may have relocated them during GC.
    for (j, h) in handles.iter().enumerate() {
        if let Some(h) = *h {
            captures[j] = Value::Object(Some(thread.native_pin_roots[h]));
        }
    }
    thread.native_pin_roots.truncate(pin_base);

    for (i, val) in captures.iter().enumerate() {
        shared.mem.heap.set_field(proxy_ref, i, *val);
    }
    // The i54-L1 retirement: the spun constructor's method events, which
    // HotSpot posts as the instance is built.
    #[cfg(feature = "experimental-debug")]
    let proxy_ref = crate::runtime::interpreter::post_lambda_proxy_constructor_events(
        shared,
        thread,
        proxy_class_id,
        proxy_ref,
        captures,
    );

    // Zero-capture call sites mint their singleton exactly once; every
    // later invocation hits the fast path above instead.
    //
    // FIRST WRITER WINS. Two threads evaluating the same cold site both miss
    // the probe above and both allocate. A plain `insert` let the second
    // overwrite the first, so the first thread kept an instance that no later
    // evaluation would ever return again -- `==` between its result and any
    // other evaluation's was `false`, where HotSpot publishes exactly one
    // `CallSite` and therefore one instance. Returning the entry that is
    // actually in the table makes every evaluation agree; the loser's
    // allocation is unreachable garbage.
    if num_captures == 0 {
        let mut cache = lambda_singleton_cache().write();
        let slot = cache
            .entry((shared.vm_identity, proxy_class_id, site_method, site_pc))
            .or_insert_with(|| Arc::new(LambdaSingletonSlot::new(proxy_ref)));
        // A row in the map is never retired (a retire removes it), so `None`
        // is unreachable; answering with this allocation keeps it total.
        let winner = match slot.get() {
            Some(winner) => winner,
            None => {
                slot.set(proxy_ref);
                proxy_ref
            }
        };
        return Ok(winner);
    }

    Ok(proxy_ref)
}

/// Parse the return type of a method descriptor as a class name.
/// E.g. `"(I)Ljava/util/function/Consumer;"` → `Some("java/util/function/Consumer")`
pub(in crate::runtime::invokedynamic) fn parse_return_type_class(descriptor: &str) -> Option<String> {
    let ret = descriptor.rsplit(')').next()?;
    if ret.starts_with('L') && ret.ends_with(';') {
        Some(ret[1..ret.len() - 1].to_string())
    } else {
        None
    }
}

/// Resolve a MethodHandle CP entry to a full [`MethodHandle`] struct.
///
/// Extracts reference_kind, class name, member name, and descriptor from
/// the constant pool. Handles all 9 reference kinds (field refs, method refs,
/// interface method refs).
pub fn resolve_method_handle_full(
    cp: &ConstantPool,
    mh_index: u16,
) -> Result<MethodHandle, MethodCallFailed> {
    let (ref_kind, ref_index) = match cp.get(mh_index) {
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind,
            reference_index,
        }) => (*reference_kind, *reference_index),
        _ => {
            return Err(VmError::Internal {
                message: format!("invokedynamic: cp#{mh_index} is not a MethodHandle"),
            }
            .into());
        }
    };

    let kind = MethodHandleKind::from_tag(ref_kind).ok_or_else(|| VmError::Internal {
        message: format!("invokedynamic: invalid MethodHandle reference_kind {ref_kind}"),
    })?;

    // Extract class_index and name_and_type_index from the reference entry.
    // reference_kind 1-4 reference FieldReference, 5-9 reference MethodReference
    // or InterfaceMethodReference.
    let (class_index, nat_index) = match cp.get(ref_index) {
        Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        Some(ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        _ => {
            return Err(VmError::Internal {
                message: format!(
                    "invokedynamic: MethodHandle ref cp#{ref_index} \
                     is not a Field/Method/InterfaceMethod reference"
                ),
            }
            .into());
        }
    };

    let class_name = cp
        .get_class_name(class_index)
        .ok_or_else(|| VmError::Internal {
            message: format!("invokedynamic: invalid class at cp#{class_index}"),
        })?
        .to_string();

    let (member_name, descriptor) =
        cp.get_name_and_type(nat_index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic: invalid name_and_type at cp#{nat_index}"),
            })?;

    Ok(MethodHandle {
        kind,
        class_name: Arc::from(class_name),
        member_name: Arc::from(member_name.to_string()),
        descriptor: Arc::from(descriptor.to_string()),
    })
}

/// Execute StringConcatFactory.makeConcatWithConstants.
///
/// Called after the class_manager read lock has been released.
/// Execute a `StringConcatFactory` call site.
///
/// Takes the three pieces it actually needs as **borrowed** data rather than an
/// `&IndyInfo`. That is not cosmetic: this is the hottest cached
/// `invokedynamic` shape in the VM (every `"a" + b` in every logging call,
/// `toString()`, and exception message), and the cached fast path used to
/// rebuild a whole `IndyInfo` per execution — `recipe.to_string()` +
/// `target_descriptor.to_string()` + a `Vec<String>` re-allocating **every**
/// constant arg — purely to convert the cached `Arc<str>`s into the `String`s
/// this signature demanded. That was `2 + N` heap allocations plus a `Vec` on
/// every single string concatenation, all of it discarded microseconds later.
/// Generic over `S: AsRef<str>` so the cached path can pass its
/// `&[Arc<str>]` and the bootstrap path its `&[String]` with no conversion at
/// all.
///
/// `cached_latin1_shape` (INTERP719-1) is the cached site's construction-time
/// answer of the Latin-1 shape rule; `None` where no site record exists. It is
/// read only under `CRATONVM_INTERP_CONCAT_LATIN1_SHAPE_CACHE`, else the
/// STRGAP19-7 path scans the recipe per call exactly as before.
pub(in crate::runtime::invokedynamic) fn execute_string_concat<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    recipe: &[u16],
    constant_args: &[S],
    target_descriptor: &str,
    cached_latin1_shape: Option<bool>,
) -> Result<(), MethodCallFailed> {
    // Inline storage: this runs for every interpreted `"a" + b`, and almost no
    // concat site has more than eight operands, so the argument tags, values
    // and pin slots below cost no heap allocation at all.
    let mut arg_types: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
    parse_descriptor_args_into(target_descriptor, &mut arg_types);

    // Pop arguments from the stack (pushed left-to-right, pop right-to-left).
    //
    // WP4.3 — descriptor-aware decode: for `J`-typed (long) args the
    // operand-stack slot is untagged raw bits (CompactValue::long stores
    // the i64 directly without NaN-boxing for non-collision values), and a
    // plain `pop()` decodes it via `to_value()` → `Value::Double` because
    // the bit pattern is not NaN-tagged.  String concat then formats it as
    // `0.0` / a denormal.  Routing the pop through `decode_by_descriptor`
    // (already present in `types/src/compact_value.rs:520`) yields the
    // declared `Value::Long(...)` instead, which formats correctly.
    let mut arg_values: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(arg_types.len());
    for i in 0..arg_types.len() {
        let desc_byte = arg_types
            .get(arg_types.len() - 1 - i)
            .copied()
            .unwrap_or('L') as u8;
        // BC SM2-family fix (2026-07-22): use the long-mark-aware pop
        // (`pop_arg_for_descriptor_checked`, already relied on by
        // `pop_coerced_invoke_args_virtual`/`decode_arg_kind_aware` for
        // invoke-argument marshalling) instead of a plain `pop_compact()` +
        // `decode_by_descriptor()`. A `J`-descriptor arg whose raw bits
        // collide with the NaN-tag space AND whose masked payload fits in
        // 32 bits (e.g. `-1125899906842623L` == `0xFFFC000000000001`, which
        // is bit-for-bit identical to `CompactValue::int(1)`'s tagged
        // encoding) is genuinely undecodable from the bits alone —
        // `decode_by_descriptor` falls back to its i2l-widening heuristic
        // and truncates the long to its low 32 bits. The interpreter
        // already tracks provenance for exactly this case via the
        // parallel `KIND_LONG` marker pushed alongside the operand stack
        // slot; consulting it here (as the invoke-argument path already
        // does) resolves the ambiguity instead of guessing from bits.
        // String concatenation with a computed long value colliding this
        // way is not exotic — H2's `DataUtils.parseHexLong` round-trip
        // (`TestDataUtils.testParse`) hits it via plain `"" + longValue`.
        let v = thread.frames[frame_idx]
            .stack
            .pop_arg_for_descriptor_checked(desc_byte)?;
        arg_values.push(v);
    }
    arg_values.reverse();

    // STRGAP19-7 (JIT round 19 proposal wave 7, lane interp7;
    // `CRATONVM_INTERP_CONCAT_LATIN1_FAST`, default OFF): the compiled bridge's
    // exact-size Latin-1 renderer first. It runs no Java code and declines
    // (`None`) before any allocation a caller could see, so a decline leaves
    // the general renderer below with exactly today's operands, `toString()`
    // calls, exception order and output. The operands were popped above, in
    // the same order and through the same checked pop, so a pop failure
    // still raises first.
    if crate::runtime::env_cache::interp_concat_latin1_fast() {
        // INTERP719-1 (`CRATONVM_INTERP_CONCAT_LATIN1_SHAPE_CACHE`, opt-in):
        // the cached site's shape answer replaces the per-call scan.
        let shape = cached_latin1_shape
            .filter(|_| crate::runtime::env_cache::interp_concat_latin1_shape_cache());
        if let Some(str_ref) = concat_latin1::try_concat_interpreted(
            shared,
            thread,
            recipe,
            constant_args,
            &arg_types,
            &arg_values,
            shape,
        ) {
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(str_ref)))?;
            return Ok(());
        }
    }

    let str_ref = concat_values_to_string(
        shared,
        thread,
        recipe,
        constant_args,
        &arg_types,
        &arg_values,
        false,
    )?;
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(str_ref)))?;
    Ok(())
}

/// The frame-free half of [`execute_string_concat`]: render `arg_values`
/// (already popped and descriptor-typed, in left-to-right order) through
/// `recipe` into a fresh, uninterned `String`.
///
/// On `Err` every pin this function took has been released; a `toString()`
/// that throws propagates its exception exactly as HotSpot's
/// `StringConcatHelper.stringOf` does.
///
/// `tlab_latin1_result` (CONCAT5-4, the compiled bridge's per-site
/// `CRATONVM_JIT_CONCAT_TLAB_RESULT` answer; the interpreter passes `false`):
/// an all-Latin-1 result is first built from the thread's TLAB, refill
/// allowed (the same constructor the Latin-1 fast path uses), so the bridge
/// refills the TLAB an inline concat's decline found full. Same String
/// (LATIN1, `hash` 0, `hashIsZero` false) as the shared-heap arm below;
/// a refusal falls through to that arm unchanged.
/// `r19-unary-indy-concat-returns-a-fresh-string-20261008` (JIT round 19
/// wave 11, lane hoist11; HotSpot parity, default ON,
/// `CRATONVM_CONCAT_UNARY_BOOLEAN_LITERAL=0` kills it): the lone `boolean`
/// operand of a unary concat (`"" + b`: recipe exactly one argument tag, no
/// constants, operand type `Z`), or `None` for every other site.
///
/// HotSpot 25's `StringConcatFactory` serves a one-argument recipe with no
/// literal text through `unaryConcat`: a primitive `boolean` goes through
/// `String.valueOf(boolean)`, which answers the interned `"true"` /
/// `"false"` LITERAL (`R19Review5UnaryConcat` `unary-boolean-literal`
/// 200000/200000 on HotSpot). Every other unary shape is a fresh String there
/// too (`int`/`long` through `toString`, `char` through `valueOf(char)`, a
/// reference -- a `Boolean` box or a `String` included -- through
/// `StringConcatHelper.newStringOf`), so only `Z` is answered here.
pub(in crate::runtime) fn unary_boolean_concat_operand(
    recipe: &[u16],
    constant_count: usize,
    arg_types: &[char],
    arg_values: &[Value],
) -> Option<bool> {
    match (recipe, constant_count, arg_types, arg_values) {
        ([TAG_ARG_UNIT], 0, ['Z'], [Value::Int(v)]) => {
            crate::runtime::env_cache::concat_unary_boolean_literal().then_some(*v != 0)
        }
        _ => None,
    }
}

/// The longest LATIN1 concat result (`StringConcatHelper.checkOverflow`:
/// the length must stay a non-negative `int`).
pub(in crate::runtime) const CONCAT_MAX_LATIN1_UNITS: usize = i32::MAX as usize;
/// The longest UTF16 concat result (`StringConcatHelper.newArray`:
/// `length << 1` must stay a non-negative `int`, so 2^30 - 1 units).
pub(in crate::runtime) const CONCAT_MAX_UTF16_UNITS: usize = (i32::MAX as usize) >> 1;

/// `CRATONVM_CONCAT_LENGTH_OVERFLOW_OOME` (JIT round 21 wave 4, lane concat;
/// HotSpot parity, default ON, `=0` restores the old renderer, which built
/// the oversize result -- up to 8 GiB of transient units -- and then failed
/// with a plain heap `OutOfMemoryError`, or for a UTF16 result of 2^30 units
/// or more answered a String HotSpot never builds). Read only on the overflow
/// edge, never per concat, so it needs no cache and no static.
pub(in crate::runtime) fn concat_length_overflow_oome_enabled() -> bool {
    true
}

/// Does a concat result of these units overflow `StringConcatHelper`'s bound?
/// `utf16_only` is compact strings off (every String is UTF16 there).
pub(in crate::runtime) fn concat_length_overflows(units: &[u16], utf16_only: bool) -> bool {
    concat_length_overflows_by(units.len(), utf16_only, || {
        units.iter().any(|&u| u > 0xFF)
    })
}

/// [`concat_length_overflows`] over a unit count; `any_wide` (is some unit
/// above `0xFF`, i.e. is the result UTF16) is asked only past the UTF16 bound.
pub(in crate::runtime) fn concat_length_overflows_by(
    n: usize,
    utf16_only: bool,
    any_wide: impl FnOnce() -> bool,
) -> bool {
    n > CONCAT_MAX_LATIN1_UNITS || (n > CONCAT_MAX_UTF16_UNITS && (utf16_only || any_wide()))
}

/// HotSpot 25's overflow throwable, text for text
/// (`StringConcatHelper.checkOverflow` / `newArray`).
pub(in crate::runtime) fn concat_length_overflow_error() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
        message: CONCAT_LENGTH_OVERFLOW_MESSAGE.to_string(),
    }))
}

/// `StringConcatHelper`'s overflow message.
pub(in crate::runtime) const CONCAT_LENGTH_OVERFLOW_MESSAGE: &str = "Overflow: String length out of range";

pub(in crate::runtime::invokedynamic) fn concat_values_to_string<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    recipe: &[u16],
    constant_args: &[S],
    arg_types: &[char],
    arg_values: &[Value],
    tlab_latin1_result: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // `r19-unary-indy-concat-returns-a-fresh-string-20261008`: `"" + b`
    // answers the interned literal `String.valueOf(boolean)` returns
    // ([`unary_boolean_concat_operand`]). No operand is a reference, so
    // nothing needs a pin across the pool's allocation.
    if let Some(b) =
        unary_boolean_concat_operand(recipe, constant_args.len(), arg_types, arg_values)
    {
        return crate::runtime::interpreter::intern_string_literal_or_oom(
            shared,
            thread,
            if b { "true" } else { "false" },
        );
    }
    // GC-root safety: the reference-typed args now live only in the Rust
    // `arg_values` Vec — they are no longer on the (scanned) operand stack.
    // `value_to_string` invokes each object's `toString()` (real Java bytecode),
    // which is a safepoint and can trigger a young GC that *moves* the heap.
    // After a move, the raw `ObjectRef`s captured in `arg_values` are stale: a
    // later iteration would read a wrong/garbage object (e.g. concatenating two
    // non-String objects, where the first `toString()` relocates the second
    // arg). Pin every object arg in `thread.native_pin_roots`, which the
    // collector both scans as a root and forwards in place (see memory/gc.rs and
    // memory/roots.rs). We then re-read the up-to-date address from the pin slot
    // immediately before each `toString()` call. Mirrors the push/re-read/
    // truncate idiom used by the native-call and `new`/`<init>` paths in
    // vm/vm_exec.rs.
    let pin_base = thread.native_pin_roots.len();
    // arg index -> pin slot index (only present for non-null object args).
    let mut arg_pin: smallvec::SmallVec<[Option<usize>; 8]> =
        smallvec::SmallVec::with_capacity(arg_values.len());
    for v in arg_values {
        match v {
            Value::Object(Some(obj_ref)) => {
                let slot = thread.native_pin_roots.len();
                thread.native_pin_roots.push(*obj_ref);
                arg_pin.push(Some(slot));
            }
            _ => arg_pin.push(None),
        }
    }

    // Walk the recipe and build the result, accumulating UTF-16 code UNITS
    // rather than a Rust `String`.
    //
    // A Rust `String`/`str` is UTF-8 by construction and therefore cannot hold
    // an unpaired surrogate (U+D800..U+DFFF is not a Unicode scalar value). A
    // `String` accumulator here silently rewrote every one of them to U+FFFD,
    // so `"x" + lone + "y"` produced `x�y` while
    // `new String(new char[]{'x', lone, 'y'})` — which never touches Rust text
    // — came back correct. Concatenation is on the JLS's lossless path: `+`
    // copies code units, it does not validate them.
    //
    // Units also remove a transcode from the hot path rather than adding one:
    // a `String` argument used to be decoded UTF-16 -> UTF-8 on the way in and
    // re-encoded UTF-8 -> UTF-16 by `create_string_or_oom` on the way out.
    // See `string-concat-loses-unpaired-surrogates-FIXED-20260805.md`.
    //
    // Pre-sized for the literal text plus a modest guess per operand, so the
    // common short concat grows the buffer once or not at all.
    let mut result: Vec<u16> = Vec::with_capacity(recipe.len() + 16 * arg_values.len());
    let mut arg_idx = 0;
    let mut const_idx = 0;
    // r21 concat (`CRATONVM_CONCAT_LENGTH_OVERFLOW_OOME`, default ON): set once
    // the result is certain to exceed `StringConcatHelper`'s length bound.
    // From then on nothing more is appended (the accumulator is dropped), but
    // every remaining reference operand is still stringified, in order, so
    // its `toString()` side effects and exceptions happen exactly as HotSpot's
    // `stringOf` pass runs them before `mix` / `newArray` raise the overflow.
    let mut overflowed = false;

    for &tag in recipe {
        if overflowed {
            if tag == TAG_ARG_UNIT {
                if arg_idx < arg_values.len() {
                    let arg_type = arg_types.get(arg_idx).copied().unwrap_or('L');
                    if let Some(slot) = arg_pin.get(arg_idx).copied().flatten() {
                        if let Some(&obj) = thread.native_pin_roots.get(slot) {
                            let arg_val = Value::Object(Some(obj));
                            // A `String` renders as itself: no Java code.
                            if !crate::vm::is_java_lang_string(shared, obj) {
                                if let Err(e) =
                                    render_operand_checked(shared, Some(thread), &arg_val, arg_type)
                                {
                                    thread.native_pin_roots.truncate(pin_base);
                                    return Err(e);
                                }
                            }
                        }
                    }
                    arg_idx += 1;
                }
            } else if tag == TAG_CONST_UNIT {
                const_idx += 1;
            }
            continue;
        }
        if tag == TAG_ARG_UNIT {
            // Argument placeholder
            if arg_idx < arg_values.len() {
                let arg_type = arg_types.get(arg_idx).copied().unwrap_or('L');
                // Re-read the (possibly forwarded) object reference from its pin
                // slot so a GC move during a prior `toString()` doesn't leave us
                // formatting a stale pointer. Non-object args keep their value.
                let arg_val = match arg_pin.get(arg_idx).copied().flatten() {
                    Some(slot) => Value::Object(Some(
                        thread
                            .native_pin_roots
                            .get(slot)
                            .copied()
                            .unwrap_or_else(|| match arg_values[arg_idx] {
                                Value::Object(Some(o)) => o,
                                _ => unreachable!("pinned slot maps to a non-object arg"),
                            }),
                    )),
                    None => arg_values[arg_idx],
                };
                // A `String` argument is copied unit-for-unit. `value_to_string`
                // would route it through `read_java_string`, whose
                // `String::from_utf16_lossy` is exactly where the surrogate
                // died. A `toString()` result comes back as units too
                // (`RenderedOperand::Units`); only VM-formatted text
                // (primitives, wrappers, `ClassName@hash`) is Rust text, and it
                // cannot carry an unpaired surrogate in the first place.
                // Only for an object whose CLASS is `String`: the reader is
                // speculative by shape, and a `StringBuilder` operand (passed
                // as itself by a pre-JDK-19 javac) read as its whole buffer,
                // unused capacity included, instead of running `toString()`.
                // Appended in place (wave 16): reading the units out first
                // cost two heap `Vec`s per `String` operand.
                let appended = match arg_val {
                    Value::Object(Some(obj)) if crate::vm::is_java_lang_string(shared, obj) => {
                        // r21 concat: a `String` whose units would carry the
                        // result past the Latin-1 bound is not copied (a
                        // transient of up to 8 GiB of units otherwise); the
                        // result is an overflow whatever follows.
                        let past_bound = crate::vm::java_string_unit_count(&shared.mem.heap, obj)
                            .is_some_and(|n| {
                                result.len().saturating_add(n) > CONCAT_MAX_LATIN1_UNITS
                            });
                        if past_bound && concat_length_overflow_oome_enabled() {
                            overflowed = true;
                            result = Vec::new();
                            true                        } else {
                            crate::vm::append_java_string_units(&shared.mem.heap, obj, &mut result)                        }                    }
                    _ => false,
                };
                if appended {
                    // A `String` operand's units are in `result` already.
                } else if let (Value::Int(v), 'C') = (arg_val, arg_type) {
                    // A primitive `char` operand is ONE code unit, whatever it
                    // is. `value_to_string` renders it through a Rust `char`,
                    // which cannot hold a surrogate, and its fallback emitted
                    // the six characters `\ud800` -- so `"" + (char) 0xD800`
                    // had length 6 where HotSpot's has length 1.
                    result.push(v as u16);
                } else if let Some(unit) = boxed_char_unit(shared, &arg_val) {
                    // A boxed `Character` can itself BE an unpaired surrogate,
                    // and `value_to_string` renders one through a Rust `char`,
                    // which cannot hold it -- `"" + Character.valueOf('\u{d800}')`
                    // came out as `?`, not even U+FFFD. It is one unit by
                    // definition, so emit it directly.
                    result.push(unit);
                } else {
                    match render_operand_checked(shared, Some(thread), &arg_val, arg_type) {
                        Ok(RenderedOperand::Text(s)) => result.extend(s.encode_utf16()),
                        // A `toString()` result: its units, verbatim.
                        Ok(RenderedOperand::Units(units)) => result.extend_from_slice(&units),
                        Err(e) => {
                            // A `toString()` that threw. The pins are
                            // ours to release on every exit path.
                            thread.native_pin_roots.truncate(pin_base);
                            return Err(e);
                        }
                    }
                }
                arg_idx += 1;
            }
        } else if tag == TAG_CONST_UNIT {
            // Constant placeholder (from bootstrap_arguments[1..])
            if let Some(u) = constant_args.get(const_idx) {
                result.extend_from_slice(u.as_ref());
            }
            const_idx += 1;
        } else {
            // Literal recipe text. Already units, so a lone surrogate folded
            // into the recipe by javac is copied through untouched.
            result.push(tag);
        }
        // r21 concat: one compare per recipe unit; the switch is read only on
        // the (rare) overflow edge, so the hot path reads no flag.
        if !overflowed
            && result.len() > CONCAT_MAX_LATIN1_UNITS
            && concat_length_overflow_oome_enabled()
        {
            overflowed = true;
            result = Vec::new();
        }
    }

    // All `toString()` safepoints are behind us — the concatenated text is now
    // plain Rust data. Release the temporary GC pins (the only early return
    // between `pin_base` and here truncates them itself, so a single truncate
    // restores the root set exactly).
    thread.native_pin_roots.truncate(pin_base);

    // r21 concat: HotSpot 25's `StringConcatHelper` raises
    // `OutOfMemoryError("Overflow: String length out of range")` when the
    // length overflows `int` (`checkOverflow`) or when `length << coder` does
    // (`newArray`): a UTF16 result is bounded at 2^30 - 1 units. Every
    // operand has been stringified by now, as there.
    if overflowed
        || (result.len() > CONCAT_MAX_UTF16_UNITS
            && concat_length_overflows(&result, !shared.classes.latin1_coder_allowed())
            && concat_length_overflow_oome_enabled())
    {
        return Err(concat_length_overflow_error());
    }

    // CONCAT5-4: no reference is held past this point (every operand is in
    // `result` already), and the TLAB constructor performs no collection.
    if tlab_latin1_result && result.iter().all(|&u| u <= 0xFF) {
        let bytes: concat_latin1::Latin1Buf = result.iter().map(|&u| u as u8).collect();
        if let Some(obj) = crate::vm::try_create_java_string_latin1_threaded(shared, thread, &bytes)
        {
            return Ok(obj);
        }
    }

    // `StringConcatFactory` (the `"a" + b` bytecode shape) produces a brand
    // new String per the JVM spec — it must NOT be interned, otherwise `==`
    // wrongly reports identity with an equal literal.
    // `"a" + b` on a full heap must raise a catchable OutOfMemoryError, not
    // abort the VM -- see `interpreter::create_string_or_oom`.
    crate::runtime::interpreter::create_string_from_units_or_oom(shared, thread, &result)
}

/// Resolve a CONSTANT_MethodType CP entry to its descriptor string.
pub fn resolve_method_type(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index)? {
        ConstantPoolEntry::MethodType { descriptor_index } => {
            cp.get_utf8(*descriptor_index).map(|s| s.to_string())
        }
        _ => None,
    }
}

/// Resolve a string constant from the constant pool.
pub(crate) fn resolve_string_constant(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index)? {
        ConstantPoolEntry::StringReference { string_index } => {
            cp.get_utf8(*string_index).map(|s| s.to_string())
        }
        ConstantPoolEntry::Utf8(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Resolve a `makeConcatWithConstants` static constant argument (a `\u0002` /
/// TAG_CONST recipe slot) to its `String.valueOf` text.
///
/// Unlike [`resolve_string_constant`] (which only understands `String`/`Utf8`
/// and is shared with other call sites where that is the contract), the recipe
/// constants of `StringConcatFactory.makeConcatWithConstants` may be *any*
/// loadable constant. javac most often emits a folded `String` here, but the
/// spec permits `int`/`long`/`float`/`double` and `Class` constants, and a
/// faithful implementation must convert each exactly as `String.valueOf` /
/// `String.valueOf((Object) c)` would:
///   - `String`/`Utf8`           → the text verbatim
///   - `int`                     → decimal (also covers folded boolean/char as int)
///   - `long`                    → decimal
///   - `float`/`double`          → Java float/double text (`format_float`/`format_double`)
///   - `Class` (`ClassReference`) → the binary class name `String.valueOf` of a
///                                   `Class` is its `toString()`, but for the
///                                   common `String` literal case this never
///                                   applies; we render the dotted name as a
///                                   best effort rather than dropping it.
///
/// Returning `None` (→ empty string at the call site, preserving the prior
/// fail-soft behaviour) only for kinds that cannot legally appear as a recipe
/// constant.
/// [`resolve_concat_constant`] in UTF-16 code **units**.
///
/// A `CONSTANT_Utf8` entry may contain lone surrogates -- Java's modified UTF-8
/// encodes them individually and javac emits them for any string literal
/// holding one (ANTLR's `_serializedATN` is the standard example). The parser
/// keeps the exact units for such entries in a side table, because the
/// `Arc<str>` form cannot represent them; `get_utf8_wide` is `Some` only for
/// those entries and `None` for the overwhelmingly common lossless case.
///
/// Both the recipe and the `TAG_CONST` slots go through here, so a lone
/// surrogate written as a *literal* survives concatenation exactly as one
/// arriving as a runtime argument does. Before this, only the argument path was
/// lossless and `"x\uD801y" + n` still produced U+FFFD.
pub(in crate::runtime::invokedynamic) fn resolve_concat_constant_units(cp: &ConstantPool, index: u16) -> Option<Vec<u16>> {
    let wide = match cp.get(index) {
        Some(ConstantPoolEntry::StringReference { string_index }) => {
            cp.get_utf8_wide(*string_index)
        }
        Some(ConstantPoolEntry::Utf8(_)) => cp.get_utf8_wide(index),
        _ => None,
    };
    if let Some(units) = wide {
        return Some(units.to_vec());
    }
    // Every other loadable-constant kind (int/long/float/double/Class) is
    // produced as ASCII text by `String.valueOf`, so the UTF-8 form is lossless.
    resolve_concat_constant(cp, index).map(|s| s.encode_utf16().collect())
}

pub(in crate::runtime::invokedynamic) fn resolve_concat_constant(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index)? {
        ConstantPoolEntry::StringReference { string_index } => {
            cp.get_utf8(*string_index).map(|s| s.to_string())
        }
        ConstantPoolEntry::Utf8(s) => Some(s.to_string()),
        ConstantPoolEntry::Integer(v) => Some(v.to_string()),
        ConstantPoolEntry::Long(v) => Some(v.to_string()),
        ConstantPoolEntry::Float(v) => Some(format_float(*v)),
        ConstantPoolEntry::Double(v) => Some(format_double(v.get())),
        ConstantPoolEntry::ClassReference { name_index } => cp
            .get_utf8(*name_index)
            .map(|s| format!("class {}", s.replace('/', "."))),
        _ => None,
    }
}

/// The argument position from which a bootstrap's trailing `Object[]`
/// parameter collects the bootstrap arguments, or `None` when they are passed
/// positionally. `shape` is [`descriptor_param_count_and_last_is_object_array`]
/// of the bootstrap descriptor; `arg_count` counts the whole flat list
/// (`Lookup`, name, type, static arguments).
///
/// The `invokeWithArguments` rule for a varargs collector (and
/// `constants::invoke_condy_bootstrap_pinned`'s step 5): collect everything
/// from the array's position on — zero arguments give an empty array, one
/// scalar a one-element array — unless there is exactly one argument there
/// and it already is an array or `null`. Before this, only a list LONGER than
/// the parameter list was packed, so one static argument reached the
/// `Object[]` slot as a bare scalar and none left the slot missing.
pub(in crate::runtime::invokedynamic) fn bootstrap_varargs_pack_start(
    shape: Option<(usize, bool)>,
    arg_count: usize,
    last_is_array_or_null: bool,
) -> Option<usize> {
    let Some((param_count, true)) = shape else {
        return None;
    };
    let fixed = param_count.checked_sub(1)?;
    if arg_count < fixed || (arg_count == param_count && last_is_array_or_null) {
        return None;
    }
    Some(fixed)
}

