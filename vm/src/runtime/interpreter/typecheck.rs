// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Type compatibility for `checkcast` / `instanceof` / `aastore`.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: lambda proxy type-check for checkcast/instanceof`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;


/// Public re-export for JIT helpers — see [`lambda_proxy_satisfies`].
pub fn lambda_proxy_satisfies_public(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_id: ClassId,
) -> bool {
    lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
}

/// Public re-export for JIT helpers — see [`synthetic_implements`].
pub fn synthetic_implements_public(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    synthetic_implements(shared, obj_class_id, target_class_name)
}

/// Instance-aware `instanceof` / `checkcast` admission for an annotation proxy.
///
/// Every annotation proxy shares the synthetic
/// `java/lang/annotation/AnnotationProxy` ClassId, so the class-only
/// `synthetic_implements` path can only affirm the generic
/// `java.lang.annotation.Annotation` supertype. The proxy's *actual* annotation
/// interface lives on the heap object (slot 0 = the `Lpkg/Type;` descriptor),
/// so a precise check requires the instance. Returns `true` iff `obj_ref` is an
/// annotation proxy and `target_class_name` is `java/lang/annotation/Annotation`
/// or the proxy's own annotation interface. This replaces the old blanket
/// "proxy is instanceof every annotation" behaviour that broke Log4j2 plugin
/// injection (a `@Required` proxy passed `instanceof @PluginBuilderAttribute`).
pub fn annotation_proxy_satisfies_target(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    target_class_name: &str,
) -> bool {
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let is_proxy = shared
        .classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
        .unwrap_or(false);
    if !is_proxy {
        return false;
    }
    if target_class_name == "java/lang/annotation/Annotation" {
        return true;
    }
    // Slot 0 holds the annotation's type descriptor, e.g. `Lpkg/Type;`.
    if let Value::Object(Some(desc_ref)) = shared.mem.heap.get_field(obj_ref, 0) {
        if let Some(desc) = read_java_string(&shared.mem.heap, desc_ref) {
            let internal = desc
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .unwrap_or(&desc);
            return internal == target_class_name;
        }
    }
    false
}

/// `instanceof` / `checkcast` admission for a `Proxy$Instance` heap object.
///
/// Returns `true` iff `obj_ref` is a dynamic proxy AND `target_class_name`
/// names one of the interfaces the proxy was created with (or a superinterface
/// of one of them). Always-implicit constants (`java/io/Serializable`,
/// `java/lang/Object`) also match per `java.lang.reflect.Proxy` spec.
///
/// The proxy stores its interfaces array at slot
/// [`crate::runtime::proxy::PROXY_FIELD_INTERFACES`] (a `Class[]`).
///
/// See also: [`synthetic_implements`] above for the rationale this lives at
/// the call site (per-instance proxy admission can't be decided from a
/// `class_id` alone, since every proxy lands on the same `Proxy$Instance`
/// ClassId).
pub(crate) fn proxy_instance_satisfies_target(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
    target_class_name: &str,
) -> bool {
    use crate::runtime::proxy::PROXY_FIELD_INTERFACES;

    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
    let obj_name = match shared.classes.class_manager.read().get_class(obj_class_id) {
        Some(c) => c.name.to_string(),
        None => return false,
    };
    let is_proxy = &*obj_name == "java/lang/reflect/Proxy$Instance"
        || class_chain_reaches_proxy_instance(shared, obj_class_id);
    if !is_proxy {
        return false;
    }

    // Always-true targets per the `Proxy` contract.
    if target_class_name == "java/io/Serializable" || target_class_name == "java/lang/Object" {
        return true;
    }

    // A real-super generated `$ProxyN` has just the inherited handler field
    // at slot 0; its interfaces are declared on the class. Do not probe the
    // synthetic interfaces slot on it: that is out of bounds and this helper
    // is hot in Spring's conversion/binding path.
    let proxy_cid = shared.mem.heap.class_id_of(obj_ref);
    let has_iface_slot = shared
        .classes
        .class_manager
        .read()
        .get_class(proxy_cid)
        .map(|c| c.num_total_fields >= 2)
        .unwrap_or(false);
    if !has_iface_slot {
        return obj_name == "java/lang/reflect/Proxy$Instance";
    }

    let interfaces_arr = match shared.mem.heap.get_field(obj_ref, PROXY_FIELD_INTERFACES) {
        cratonvm_types::Value::Object(Some(a)) => a,
        _ => {
            // No CratonVM-internal interfaces array in slot 1. For a genuine
            // generated `$ProxyN` the proxied interfaces are recorded in the
            // class itself, so the `is_subclass_of(obj_class, target)` check at
            // the `instanceof`/`checkcast` call site is authoritative — a `true`
            // here would make the proxy `instanceof` EVERYTHING. That is exactly
            // the spring-bug-08 regression: a proxy deserialized via the
            // serialization path restores its handler (slot 0) but NOT this
            // internal interfaces slot, so `proxy instanceof AbstractAssert`
            // wrongly became true and tripped AssertJ's `isEqualTo` guard,
            // collapsing `SerializableTypeWrapperTests` to 1/8. Defer to the
            // class-declared interfaces by returning `false`; only the bare
            // `Proxy$Instance` shim (which has neither a slot-1 array nor its
            // own declared interface set) keeps the old liberal rule.
            return obj_name == "java/lang/reflect/Proxy$Instance";
        }
    };
    // Source the proxy's interface set. The synthetic 3-slot layout stores the
    // `Class[]` at slot 1 (`PROXY_FIELD_INTERFACES`); the real-super layout
    // (proxy-real-classfile migration: the generated `$ProxyN` extends
    // `java.lang.reflect.Proxy`, sole field `h` at slot 0) has no such slot, so
    // its interfaces come from the proxy class's own declared interfaces. Decide
    // by the receiver's field count so we never read slot 1 out of bounds on the
    // 1-field real-super proxy — that OOB read returned null and fell into the
    // liberal "matches every target" fallback below, which made a real-super
    // proxy report `instanceof` TRUE for ARBITRARY types (e.g. `java.lang.Class`).
    // That in turn routed the proxy through `ObjectOutputStream.writeClass` → a
    // null-`name` class descriptor → NPE, breaking proxy serialization
    // round-trips (proxy-real-classfile Increment 4 soak). Mirrors the same
    // slot-1→declared-interfaces fix applied to
    // `proxy_resolve_declaring_class_mirror`.
    let mut iface_cids: Vec<ClassId> = Vec::new();
    if has_iface_slot {
        match shared.mem.heap.get_field(obj_ref, PROXY_FIELD_INTERFACES) {
            cratonvm_types::Value::Object(Some(arr)) => {
                let n = shared.mem.heap.array_length(arr);
                for i in 0..n {
                    // Use the mirror→ClassId mapping we already maintain (slot 0
                    // on real-JDK Class is `cachedConstructor` — too fragile).
                    if let Ok(cratonvm_types::Value::Object(Some(m))) =
                        shared.mem.heap.get_array_element(arr, i)
                    {
                        if let Some(cid) = crate::vm::class_id_from_mirror(shared, m) {
                            iface_cids.push(cid);
                        }
                    }
                }
            }
            _ => {
                // Synthetic-layout proxy with no interfaces array recorded —
                // preserve the historical liberal rule so we don't regress
                // proxies that never went through `Proxy.newProxyInstance`.
                return true;
            }
        }
    };
    let n = shared.mem.heap.array_length(interfaces_arr);
    let target_cid = shared
        .classes
        .class_manager
        .write()
        .load_class(target_class_name)
        .ok();
    for i in 0..n {
        let mirror = match shared.mem.heap.get_array_element(interfaces_arr, i) {
            Ok(cratonvm_types::Value::Object(Some(m))) => m,
            _ => continue,
        };
        // Read the `name` String off the Class mirror via the heap (slot 0
        // on real-JDK Class is `cachedConstructor` — too fragile). Use the
        // mirror→ClassId mapping we already maintain.
        let iface_cid = match crate::vm::class_id_from_mirror(shared, mirror) {
            Some(cid) => cid,
            None => continue,
        };
        // Direct identity match.
        if Some(iface_cid) == target_cid {
            return true;
        }
        // Superinterface walk: target is a superinterface of `iface_cid`?
        if let Some(tcid) = target_cid {
            if shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(iface_cid, tcid)
            {
                return true;
            }
        }
        // Name fallback (synthetic interfaces that may not be loaded yet).
        if let Some(iface_class) = shared.classes.class_manager.read().get_class(iface_cid) {
            if &*iface_class.name == target_class_name {
                return true;
            }
        }
    }
    false
}

/// Check if a lambda proxy object satisfies a target class. Lambda proxy ClassIds
/// (>= 0x8000_0000) are not in the class store, so normal `is_subclass_of` always
/// returns false. Instead, we look up the proxy's `functional_interface` and check
/// if that interface is a subclass of the target.
pub(super) fn lambda_proxy_satisfies(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_id: ClassId,
) -> bool {
    let dbg_aci = crate::runtime::env_cache::dbg_loader_trace();
    let proxies = shared.classes.lambda_proxies.read();
    if dbg_aci && !proxies.contains_key(&obj_class_id) {
        eprintln!(
            "[LOADER-TRACE] lambda_proxy_satisfies: obj_class_id={obj_class_id:?} NOT a registered lambda proxy target_class_id={target_class_id:?}"
        );
    }
    if let Some(call_site) = proxies.get(&obj_class_id) {
        let iface_name = call_site.functional_interface.clone();
        drop(proxies); // release lock before loading
        if dbg_aci {
            let target_name_dbg = shared
                .classes
                .class_manager
                .read()
                .get_class(target_class_id)
                .map(|c| c.name.to_string());
            eprintln!(
                "[LOADER-TRACE] lambda_proxy_satisfies: obj_class_id={obj_class_id:?} iface_name={iface_name} target_class_id={target_class_id:?} target_name={target_name_dbg:?}"
            );
        }
        // `LambdaMetafactory.altMetafactory` with `FLAG_SERIALIZABLE` -- every JDK
        // `Comparator.comparing*`, and any `(Iface & Serializable)` intersection
        // cast -- adds `java.io.Serializable` to the spun proxy's interfaces. A
        // plain `metafactory` lambda does NOT implement it: on real HotSpot
        // `(Serializable) (Supplier<String>) () -> "x"` throws ClassCastException.
        // This used to accept Serializable universally because the flag was not
        // tracked, which made every CratonVM lambda look serializable. It is
        // recorded now, so ask the one owner of the rule.
        let target_name = shared
            .classes
            .class_manager
            .read()
            .get_class(target_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if &*target_name == "java/io/Serializable" {
            return shared.lambda_proxy_serializability(obj_class_id)
                != cratonvm_native_api::LambdaSerializability::NotSerializable;
        }
        // `altMetafactory`'s FLAG_MARKERS block names ADDITIONAL interfaces the
        // spun proxy implements on top of the functional interface. The proxy's
        // synthetic ClassId has no ClassStore `interfaces` vector to hold them,
        // so they live in a side table beside `lambda_proxies`; ask it before
        // falling through to the functional interface, or an intersection-cast
        // lambda fails its own `instanceof` / `checkcast`.
        if crate::runtime::invokedynamic::lambda_proxy_marker_satisfies(
            shared,
            obj_class_id,
            target_class_id,
            &target_name,
        ) {
            return true;
        }
        // A lambda proxy is defined for precisely this functional-interface
        // name. Its synthetic VM-only ClassId has no ClassStore hierarchy, and
        // a global reload can select a different loader's mirror during forked
        // test execution. The call-site metadata is authoritative here.
        if iface_name.as_ref() == target_name {
            return true;
        }
        let load_result = shared.load_class_concurrent(&iface_name);
        if let Ok(iface_id) = load_result {
            // Plain ClassId-based `is_subclass_of` fails when `iface_name`'s
            // globally-resolved copy (e.g. `AotApplicationContextInitializer`,
            // first loaded under the Application loader) extends a DIFFERENT
            // loader's copy of `target_class_name` than the one THIS checkcast
            // resolved (e.g. the fork loader's own `ApplicationContextInitializer`,
            // `target_class_id`) — both are genuinely "ApplicationContextInitializer"
            // by name, just different per-loader Class objects. Fall back to the
            // same name-based hierarchy walk `checkcast` itself already uses for
            // non-lambda receivers (see `loader_aware_name_assignable`'s call site
            // in `Instruction::Checkcast`) instead of only trusting ClassId identity.
            return shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(iface_id, target_class_id)
                || loader_aware_name_assignable(shared, iface_id, target_class_id, &target_name);
        }
    }
    false
}

pub(super) fn loader_aware_name_assignable(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    if !crate::runtime::env_cache::loader_aware_resolution()
        || is_global_resolution_namespace(target_class_name)
    {
        return false;
    }

    let cm = shared.classes.class_manager.read();
    let Some(obj_class) = cm.get_class(obj_class_id) else {
        return false;
    };
    let Some(target_class) = cm.get_class(target_class_id) else {
        return false;
    };

    if &*obj_class.name == target_class_name && &*target_class.name == target_class_name {
        return true;
    }
    let mut queue: Vec<ClassId> = Vec::new();
    let mut current = Some(obj_class_id);
    while let Some(cid) = current {
        let Some(class) = cm.class_store.get(cid) else {
            break;
        };
        // The resolved target can be a same-named class mirror from a
        // different loader, not only an interface. Compare the structural
        // superclass chain by binary name before relying on ClassId identity.
        if &*class.name == target_class_name {
            return true;
        }
        queue.extend_from_slice(&class.interfaces);
        current = class.superclass;
    }

    if !target_class.is_interface() {
        return false;
    }

    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(iface_id) = queue.pop() {
        if seen.contains(&iface_id) {
            continue;
        }
        seen.push(iface_id);
        let Some(iface) = cm.class_store.get(iface_id) else {
            continue;
        };
        if &*iface.name == target_class_name {
            return true;
        }
        queue.extend_from_slice(&iface.interfaces);
    }

    false
}

// ---------------------------------------------------------------------------
// Helper: array type compatibility for checkcast/instanceof
// ---------------------------------------------------------------------------

/// Compute the JVM array descriptor (e.g. `"[I"`, `"[Ljava/lang/String;"`)
/// for a heap object that is known to be an array. Returns `None` if the
/// object is not actually an array.
pub(crate) fn array_descriptor_of(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<String> {
    if shared.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let et = shared.mem.heap.element_type_of(obj_ref);
    match et {
        ArrayElementType::Boolean => Some("[Z".to_string()),
        ArrayElementType::Char => Some("[C".to_string()),
        ArrayElementType::Float => Some("[F".to_string()),
        ArrayElementType::Double => Some("[D".to_string()),
        ArrayElementType::Byte => Some("[B".to_string()),
        ArrayElementType::Short => Some("[S".to_string()),
        ArrayElementType::Int => Some("[I".to_string()),
        ArrayElementType::Long => Some("[J".to_string()),
        ArrayElementType::Reference => {
            // The class_id on a Reference array holds the component class id.
            let comp_id = shared.mem.heap.class_id_of(obj_ref);
            let comp_name = shared
                .classes
                .class_manager
                .read()
                .get_class(comp_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            if comp_name.is_empty() {
                Some("[Ljava/lang/Object;".to_string())
            } else if comp_name.starts_with('[') {
                // nested array: descriptor is "[" + comp_name
                Some(format!("[{}", comp_name))
            } else {
                Some(format!("[L{};", comp_name))
            }
        }
    }
}

/// Check whether an array object (with descriptor `src_desc`) is assignment-
/// compatible with `target_name`. `target_name` may be:
///   - An array descriptor like "[I" or "[Ljava/lang/Object;".
///   - A class/interface name like "java/lang/Object", "java/io/Serializable",
///     or "java/lang/Cloneable".
pub(crate) fn array_is_assignable_to(shared: &SharedVm, src_desc: &str, target_name: &str) -> bool {
    // Lenient entry point: used by `checkcast` and `aastore`, where the native
    // array-allocation leniency (an `Object[]` standing in for a `T[]` whose real
    // element type a native path didn't preserve) must not provoke a spurious
    // `ClassCastException` / `ArrayStoreException`.
    array_is_assignable_to_impl(shared, src_desc, target_name, true)
}

/// Strict variant for the `instanceof` opcode (SBR-03). Unlike `checkcast`,
/// `instanceof` must answer precisely: `Object[] instanceof I[]` is `false`
/// because `Object` is not assignable to the interface `I`. The lenient
/// `Object[]`→`T[]` fallback used for casts is suppressed here so a genuine
/// `Object[]` is not reported as an instance of an unrelated `T[]`.
pub(crate) fn array_is_instance_of(shared: &SharedVm, src_desc: &str, target_name: &str) -> bool {
    array_is_assignable_to_impl(shared, src_desc, target_name, false)
}

pub(super) fn array_is_assignable_to_impl(
    shared: &SharedVm,
    src_desc: &str,
    target_name: &str,
    lenient: bool,
) -> bool {
    // Every array is an Object and implements Serializable + Cloneable.
    if &*target_name == "java/lang/Object"
        || target_name == "java/io/Serializable"
        || target_name == "java/lang/Cloneable"
    {
        return true;
    }
    if !target_name.starts_with('[') {
        // Array cannot be cast to arbitrary non-array class.
        return false;
    }
    if src_desc == target_name {
        return true;
    }
    // Parse: strip leading '[' from both.
    let src_rest = &src_desc[1..];
    let tgt_rest = &target_name[1..];

    // Primitive component: must match exactly.
    if src_rest.len() == 1 && "ZCBSIJFD".contains(&src_rest[..1]) {
        return src_rest == tgt_rest;
    }
    if tgt_rest.len() == 1 && "ZCBSIJFD".contains(&tgt_rest[..1]) {
        return false;
    }
    // Both are reference-component arrays. Recurse on components.
    //   [Lfoo; vs [Lbar; — extract "foo"/"bar"
    //   [[I vs [[I — nested array
    let extract_component = |desc: &str| -> Option<(bool, String)> {
        // returns (is_array, name)
        if desc.starts_with('[') {
            Some((true, desc.to_string()))
        } else if desc.starts_with('L') && desc.ends_with(';') {
            Some((false, desc[1..desc.len() - 1].to_string()))
        } else {
            None
        }
    };
    let (src_is_arr, src_comp) = match extract_component(src_rest) {
        Some(x) => x,
        None => return false,
    };
    let (tgt_is_arr, tgt_comp) = match extract_component(tgt_rest) {
        Some(x) => x,
        None => return false,
    };
    if src_is_arr && tgt_is_arr {
        return array_is_assignable_to_impl(shared, &src_comp, &tgt_comp, lenient);
    }
    if src_is_arr != tgt_is_arr {
        // One is nested array, the other is an object-component; only compatible
        // if the object component is Object/Serializable/Cloneable.
        if !src_is_arr {
            return false;
        }
        return tgt_comp == "java/lang/Object"
            || tgt_comp == "java/io/Serializable"
            || tgt_comp == "java/lang/Cloneable";
    }
    // Both are reference (non-array) component class names.
    // Lenient fallback (checkcast/aastore only): our native array-allocation
    // paths often create reference arrays with component `java/lang/Object` when
    // the runtime component type is actually a subclass (e.g.
    // `getEnumConstantsShared` returns `[Ljava/lang/Object;` but callers cast to
    // `[LEnum;`). Accept these casts so reflection/enum paths don't spuriously
    // fail. For `instanceof` (lenient == false) this is suppressed: a genuine
    // `Object[]` is NOT an instance of `I[]` (SBR-03).
    if src_comp == "java/lang/Object" {
        return lenient || tgt_comp == "java/lang/Object";
    }
    // Array casts are common on reflection API results. In particular, a
    // correctly typed `Annotation[]` is routinely widened to `Object[]` by
    // JUnit and Spring. The old path unconditionally acquired the
    // class-manager *write* lock and invoked `load_class` for both components,
    // even though these bootstrap types are already loaded. That turns every
    // such cast into a global synchronization point; an imprecise `Object[]`
    // happens to skip it through the lenient fallback above, which masked the
    // cost while violating the reflection return-type contract.
    //
    // Preserve the existing name-based, loader-agnostic semantics, but resolve
    // from the read-side class table first. `load_class_concurrent` retains the
    // old on-demand loading behavior for a genuine miss without forcing the
    // warm path through an exclusive lock.
    let resolve_component = |name: &str| {
        // Do NOT chain `.read()....or_else(|| ...load_class_concurrent...)` in
        // one expression: the `RwLockReadGuard` temporary from `.read()` is not
        // dropped until the end of the *statement*, which -- in a single
        // expression -- includes the `or_else` closure's own execution. On a
        // genuine miss, `load_class_concurrent` takes `class_manager.write()`
        // on the SAME thread that still (per that temporary-lifetime rule)
        // holds its own read guard, self-deadlocking against a non-reentrant
        // `parking_lot::RwLock`. Bind the read result to a `let` first so the
        // guard drops before any write-lock attempt.
        let found = shared
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(name);
        found.or_else(|| shared.load_class_concurrent(name).ok())
    };
    let src_id = match resolve_component(&src_comp) {
        Some(id) => id,
        None => return false,
    };
    let tgt_id = match resolve_component(&tgt_comp) {
        Some(id) => id,
        None => return false,
    };
    shared
        .classes
        .class_manager
        .read()
        .is_subclass_of(src_id, tgt_id)
}

/// JVMS §aastore covariance check: returns `true` if the (non-null) element
/// `value_ref` may be stored into the reference array `array_ref`, `false` if
/// the store must throw `ArrayStoreException`.
///
/// Shared by the interpreter `aastore` opcode and the JIT `jit_aastore` helper
/// so both enforce the same rule. Only call this for genuine `Object[]`-family
/// (reference-component) arrays with a non-null element; a `null` element is
/// always storable and primitive-component arrays never reach `aastore`.
///
/// Conservative posture: the check is *additive correctness* — it must never
/// produce a FALSE `ArrayStoreException`. Whenever the component or element type
/// cannot be determined precisely (missing class entries, synthetic class ids,
/// `java/lang/Object` component, unresolvable names) it returns `true` (allow
/// the store), matching the lenient fallbacks already in
/// [`array_is_assignable_to`]. It only returns `false` when both types resolve
/// to loaded classes AND the element is provably NOT assignable to the
/// component.
pub(crate) fn aastore_element_assignable(
    shared: &SharedVm,
    array_ref: cratonvm_types::ObjectRef,
    value_ref: cratonvm_types::ObjectRef,
) -> bool {
    // Determine the array's component descriptor by stripping the leading '['
    // from its full descriptor (e.g. "[Ljava/lang/Number;" -> "Ljava/lang/Number;").
    let array_desc = match array_descriptor_of(shared, array_ref) {
        Some(d) => d,
        // Unknown array shape (no descriptor) — fail open, allow the store.
        None => return true,
    };
    if !array_desc.starts_with('[') {
        return true;
    }
    let component = &array_desc[1..];

    // Object[] (and Serializable[]/Cloneable[]) accept any reference element.
    if component == "Ljava/lang/Object;"
        || component == "Ljava/io/Serializable;"
        || component == "Ljava/lang/Cloneable;"
    {
        return true;
    }

    // Element is itself an array → use the array-vs-array assignability rules,
    // treating the component descriptor as the target.
    if shared.mem.heap.kind_of(value_ref) == cratonvm_types::ObjectKind::Array {
        let elem_desc = match array_descriptor_of(shared, value_ref) {
            Some(d) => d,
            None => return true,
        };
        // `array_is_assignable_to` wants the target as an array descriptor or a
        // class/interface name. A reference component "L...;" must be unwrapped
        // to a bare class name; an array component "[..." is passed verbatim.
        if component.starts_with('[') {
            return array_is_assignable_to(shared, &elem_desc, component);
        }
        if component.starts_with('L') && component.ends_with(';') {
            let comp_name = &component[1..component.len() - 1];
            return array_is_assignable_to(shared, &elem_desc, comp_name);
        }
        return true;
    }

    // Element is a plain object. The component must be a reference type "L...;"
    // (an array component would not accept a non-array element — but fail open
    // rather than throw, to avoid regressions from imprecise component info).
    if !(component.starts_with('L') && component.ends_with(';')) {
        return true;
    }
    let comp_name = &component[1..component.len() - 1];

    // Resolve the element's runtime class id; an unknown/synthetic class id
    // (no loaded class entry) is treated as assignable (fail open).
    let value_class_id = shared.mem.heap.class_id_of(value_ref);
    if value_class_id == ClassId::new(0) {
        return true;
    }
    let array_component_class_id = shared.mem.heap.class_id_of(array_ref);
    let comp_id = {
        let cm = shared.classes.class_manager.read();
        if array_component_class_id != ClassId::new(0) {
            cm.get_class(array_component_class_id)
                .filter(|c| &*c.name == comp_name)
                .map(|_| array_component_class_id)
        } else {
            None
        }
    }
    .or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_class(comp_name, array_component_class_id)
    })
    .unwrap_or_else(
        || match shared.classes.class_manager_write().load_class(comp_name) {
            Ok(id) => id,
            Err(_) => ClassId::new(0),
        },
    );
    if comp_id == ClassId::new(0) {
        return true;
    }
    // The element class must exist in the hierarchy; if not, fail open.
    {
        let cm = shared.classes.class_manager.read();
        if cm.get_class(value_class_id).is_none() {
            return true;
        }
        // ONE by-name walk, not two checks.
        //
        // `array_descriptor_of` preserves only the component *name*, not its
        // defining-loader ClassId. When a forked loader owns a same-named copy,
        // the global lookup above can resolve the app copy and make a valid
        // `ChildSegment[] <- ChildSegment` store look incompatible — so the
        // component identity is ambiguous here and the predicate's documented
        // fail-open posture applies. The same is true one level up: the stored
        // value can be a *subclass* whose recorded superclass edge points at
        // another same-named mirror.
        //
        // This walk covers both, because it starts at `value_class_id` itself:
        // its first iteration is exactly the `value_class.name == comp_name`
        // test that used to sit above it as a separate early return. That early
        // return was measured to be dead weight — disabling the walk fails
        // `aastore_fails_open_across_a_split_loaders_two_copies_of_one_name`,
        // disabling the early return changes nothing — and two checks that read
        // as independent defences when only one of them can ever fire is worse
        // than one check that says what it does.
        let mut current = Some(value_class_id);
        while let Some(id) = current {
            let Some(class) = cm.get_class(id) else {
                break;
            };
            if &*class.name == comp_name {
                return true;
            }
            current = class.superclass;
        }
        // Component is an INTERFACE → fail open. Proving a value implements an
        // interface is unreliable in this VM (dynamic proxies, annotation
        // proxies, and synthetic classes implement interfaces at runtime / by
        // name, invisibly to the static hierarchy). A genuine ArrayStoreException
        // essentially always involves a concrete-class component (Number[],
        // String[], …); for an interface[] we don't risk a spurious throw.
        if cm.get_class(comp_id).map_or(false, |c| c.is_interface()) {
            return true;
        }
        // Provably assignable iff value's class is a subclass/subtype of the
        // component class (interfaces handled by `is_subclass_of`).
        if cm.is_subclass_of(value_class_id, comp_id) {
            return true;
        }
    }
    // Both types resolved and the element is NOT a subtype of the component:
    // potentially an ArrayStoreException. Several escape hatches keep us from
    // throwing a SPURIOUS one on the VM's imprecise synthetic / dynamic-proxy
    // types, whose true interface set is not recorded in the static hierarchy:
    //   - synthetic lambda/proxy class ids (>= 0x8000_0000) never appear in the
    //     loaded hierarchy, so `is_subclass_of` cannot vouch for them.
    if value_class_id.as_u32() >= 0x8000_0000 {
        return true;
    }
    //   - dynamic proxies (java.lang.reflect.Proxy instances) and annotation
    //     proxies (java/lang/annotation/AnnotationProxy) implement their target
    //     interfaces at RUNTIME, invisibly to `is_subclass_of`. Storing one into
    //     an interface[] is legal on a real JVM (regression: hibernate-smoke
    //     stored an AnnotationProxy into an annotation-type array). Fail open.
    {
        let cm = shared.classes.class_manager.read();
        if let Some(cls) = cm.get_class(value_class_id) {
            let vn: &str = &*cls.name;
            if vn == "java/lang/annotation/AnnotationProxy"
                || vn.ends_with("AnnotationProxy")
                || vn.contains("$Proxy")
            {
                return true;
            }
        }
    }
    if class_chain_reaches_proxy_instance(shared, value_class_id) {
        return true;
    }
    //   - synthetic classes whose interface relationships are name-based only
    //     (HashMap$Entry, etc.) honour the existing name-based fallback.
    if synthetic_implements(shared, value_class_id, comp_name) {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Helper: walk a class's superclass chain by name to detect Proxy$Instance
// ---------------------------------------------------------------------------

/// WP2.5 — walks the superclass chain of `class_id` looking for
/// `java/lang/reflect/Proxy$Instance` or the real-JDK
/// `java/lang/reflect/Proxy` base class. Returns `true` if found within
/// `MAX_DEPTH` hops.
///
/// Used by the cast/instanceof and dispatch hooks below to extend their
/// "is this a proxy?" check from a literal name match to "literal match
/// OR extends `Proxy$Instance`" — matters once the WP2.5-A bytecode
/// emitter starts producing per-(loader, ifaces) `$ProxyN` classes that
/// extend `Proxy$Instance` (and the receiver's runtime class is the
/// generated `$ProxyN`, not the abstract super). The literal-name fast
/// path stays in the caller; this helper only runs on the slow path so
/// the cost is zero on every non-proxy dispatch.
///
/// We could call `class_manager.is_subclass_of(child, parent_id)`, but
/// that needs the ClassId of `Proxy$Instance` which forces a
/// `load_class("Proxy$Instance")` on the slow path. Walking by name is
/// simpler, lock-scoped, and depth-bounded against pathological cycles
/// in user-loaded class graphs.
pub(crate) fn class_chain_reaches_proxy_instance(shared: &SharedVm, class_id: ClassId) -> bool {
    const MAX_DEPTH: usize = 32;
    const PROXY_INSTANCE: &str = "java/lang/reflect/Proxy$Instance";
    const REAL_PROXY_BASE: &str = "java/lang/reflect/Proxy";

    let cm = shared.classes.class_manager.read();
    let mut current = Some(class_id);
    for _ in 0..MAX_DEPTH {
        let cid = match current {
            Some(c) => c,
            None => return false,
        };
        let class = match cm.get_class(cid) {
            Some(c) => c,
            None => return false,
        };
        if &*class.name == PROXY_INSTANCE || &*class.name == REAL_PROXY_BASE {
            return true;
        }
        // Stop early once we hit Object — Proxy$Instance sits below it
        // by construction, so going further is wasted work.
        if &*class.name == "java/lang/Object" {
            return false;
        }
        current = class.superclass;
    }
    false
}

// ---------------------------------------------------------------------------
// Helper: name-based type compatibility for synthetic classes
// ---------------------------------------------------------------------------

/// Synthetic classes (HashMap$Entry, etc.) may not have proper interface
/// relationships in the ClassStore because they were created in Rust without
/// loading a real .class file. This function provides a name-based fallback
/// for common patterns where a concrete inner class should satisfy an interface.
pub(super) fn synthetic_implements(shared: &SharedVm, obj_class_id: ClassId, target_class_name: &str) -> bool {
    let obj_class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .map(|c| c.name.to_string());
    let obj_name = match obj_class_name {
        Some(n) => n,
        None => return false,
    };

    if obj_name == "java/lang/foreign/DowncallHandle"
        && target_class_name == "java/lang/invoke/MethodHandle"
    {
        return true;
    }

    // `System.getLogger()` returns a `cratonvm/internal/SystemLogger`, a
    // CONCRETE synthetic class. Concrete is the whole point: the receiver used
    // to be allocated with the `java/lang/System$Logger` INTERFACE as its
    // class, which left it inert from both directions at once — native lookup
    // drops interface-declared instance natives, and the interface's own
    // methods are abstract, so neither a native nor bytecode served it.
    //
    // The cost of that fix is that the receiver no longer carries the
    // interface in its type hierarchy, so `instanceof System.Logger` and
    // `checkcast` would fail. Nothing hits that today (javac emits no
    // checkcast for `System.getLogger(..)`, whose declared return type is
    // already `System.Logger`), but a caller that stashes one in an `Object`
    // and casts it back would — and that failure would be baffling. Declare
    // the relationship, which is in fact true.
    if obj_name == "cratonvm/internal/SystemLogger"
        && target_class_name == "java/lang/System$Logger"
    {
        return true;
    }

    // Map.Entry implementations
    if target_class_name == "java/util/Map$Entry" {
        return matches!(
            &*obj_name,
            "java/util/HashMap$Entry"
                | "java/util/HashMap$Node"
                | "java/util/AbstractMap$SimpleEntry"
                | "java/util/AbstractMap$SimpleImmutableEntry"
                | "java/util/TreeMap$Entry"
                | "java/util/LinkedHashMap$Entry"
                | "java/util/Hashtable$Entry"
                | "java/util/WeakHashMap$Entry"
                | "java/util/concurrent/ConcurrentHashMap$Node"
        );
    }

    // Iterator implementations
    if target_class_name == "java/util/Iterator" {
        return obj_name.contains("$Itr")
            || obj_name.contains("$Iterator")
            || obj_name.contains("$KeyItr")
            || obj_name.contains("$ValueItr")
            || obj_name.contains("$EntryItr");
    }

    // Iterable implementations (all Collection types)
    //
    // `java/util/Collections$*` (the utility class's nested helper/view
    // types — SingletonMap, UnmodifiableMap, SingletonSet, ...) must be
    // excluded from this substring probe: "Collections" itself contains the
    // substring "Collection", so e.g. `Collections$SingletonMap` (a Map,
    // NOT a Collection) matched `obj_name.contains("Collection")` below and
    // was misreported as `instanceof java.util.Collection`/`Iterable`. That
    // broke Groovy's `DefaultTypeTransformation.asCollection`, which checks
    // `instanceof Collection` before `instanceof Map` — it took the
    // Collection branch, cast the SingletonMap to Collection unchanged, and
    // called `.iterator()` directly on it, crashing GroovyMarkupConfigurer
    // bean creation with `NoSuchMethodError: Collections$SingletonMap.
    // iterator()`. This name-based fallback only exists for genuinely
    // synthetic classes with no real interface data (see the function doc);
    // every `Collections$*` class reaching this point is either a REAL
    // loaded JDK class (whose hierarchy the earlier `is_subclass_of` check
    // already resolved precisely) or one of CratonVM's own `cratonvm/
    // internal/Unmodifiable*` stamps (which declare their own accurate
    // interfaces in `vm_init.rs`), so it never needs this heuristic.
    if obj_name.starts_with("java/util/Collections$") {
        return false;
    }
    if target_class_name == "java/lang/Iterable" {
        return obj_name.starts_with("java/util/")
            && (obj_name.contains("List")
                || obj_name.contains("Set")
                || obj_name.contains("Queue")
                || obj_name.contains("Deque")
                || obj_name.contains("Collection"));
    }

    // Collection / Set / List supertypes. These MUST stay distinct: a Set is
    // not a List and vice-versa. The previous shape admitted any List/Set/Queue
    // name for ALL THREE targets, so `x instanceof List` returned true for a
    // HashSet / RegularEnumSet (name contains "Set") — which broke real code
    // that branches on `parameters instanceof List` (JUnit's
    // Parameterized$RunnersFactory.allParameters over an `EnumSet`-typed
    // @Parameters: it took the List branch and called `enumSet.get(0)` →
    // NoSuchMethodError RegularEnumSet.get(I)). Match each interface only
    // against the class-name family that actually implements it.
    if obj_name.starts_with("java/util/") {
        match target_class_name {
            "java/util/Collection" | "java/lang/Iterable" => {
                if obj_name.contains("List")
                    || obj_name.contains("Set")
                    || obj_name.contains("Queue")
                    || obj_name.contains("Deque")
                    || obj_name.contains("Collection")
                {
                    return true;
                }
            }
            "java/util/List" => {
                // Lists only (ArrayList, LinkedList, CopyOnWriteArrayList,
                // Arrays$ArrayList, …). A Set/Queue is NOT a List.
                if obj_name.contains("List") {
                    return true;
                }
            }
            "java/util/Set" => {
                if obj_name.contains("Set") {
                    return true;
                }
            }
            "java/util/Queue" | "java/util/Deque" => {
                if obj_name.contains("Queue") || obj_name.contains("Deque") {
                    return true;
                }
            }
            _ => {}
        }
    }

    // Comparable
    if target_class_name == "java/lang/Comparable" {
        return matches!(
            &*obj_name,
            "java/lang/String"
                | "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Double"
                | "java/lang/Float"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Character"
                | "java/lang/Boolean"
        );
    }

    // NOTE — the dynamic-proxy admission rule lives in the callers (see
    // `proxy_instance_satisfies_target` below), where the proxy *instance*
    // is in scope. We can't decide it here without the instance because a
    // proxy's interface set is per-instance (stored on the heap object),
    // not per-class — every proxy lands on the same synthetic
    // `Proxy$Instance` ClassId.

    // Annotation proxy — name-based path only knows the shared
    // `AnnotationProxy` ClassId, so it can only affirm the generic
    // `java.lang.annotation.Annotation` supertype that EVERY proxy satisfies.
    // The SPECIFIC annotation-interface check is instance-aware (the proxy's
    // real type is stored on the heap object) and handled by
    // `annotation_proxy_satisfies_target`. The previous `|| true` made a proxy
    // `instanceof` EVERY annotation interface, so frameworks that distinguish
    // annotation types via instanceof/cast misbehaved — e.g. Log4j2 plugin
    // injection cast a `@Required` proxy to `@PluginBuilderAttribute`, read an
    // empty `value()`, fell back to the field name, and produced a null
    // attribute ("loggerName has invalid value null").
    if &*obj_name == "java/lang/annotation/AnnotationProxy" {
        return target_class_name == "java/lang/annotation/Annotation";
    }

    // Object is always a valid target
    if target_class_name == "java/lang/Object" {
        return true;
    }

    false
}
