#[allow(unused_imports)]
use super::*;

/// Post-GC remap for the annotation-proxy cache (companion to
/// [`gc_scan_annotation_proxy_roots`]). Repoints each cached `ObjectRef` to its
/// new address after a moving collection.
pub fn gc_update_annotation_proxy_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    ANNOTATION_PROXY_CACHE.with(vm_identity, |table| {
        for obj_ref in table.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    });

    ANNOTATION_PROXY_CHILD_ROOTS.with(vm_identity, |table| {
        let mut remapped = FxHashMap::default();
        for (proxy_addr, mut roots) in table.drain() {
            for root in roots.iter_mut() {
                let old_addr = root.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
            let new_proxy_addr = *pointer_map.get(&proxy_addr).unwrap_or(&proxy_addr);
            remapped.insert(new_proxy_addr, roots);
        }
        *table = remapped;
    });

    // bug nb-lib-gckeys В§2: remap the last-proxy interfaces array alongside
    // the annotation-proxy cache (it shares this hook). Without the repoint
    // the shared-mirror `getInterfaces()` would hand back a stale pointer.
    PROXY_LAST_INTERFACES.with(vm_identity, |cell| {
        if let Some(arr) = cell.as_mut() {
            let old_addr = arr.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *arr = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    });
    gc_update_annotation_jdk_handlers(vm_identity, pointer_map);
}

/// Annotation instances are materialised as REAL `$ProxyN` proxies by default,
/// matching HotSpot (`annotation.getClass()` is the generated proxy class, not
/// the annotation interface). The synthetic `AnnotationProxy` still carries the
/// member data and acts as the proxy's `InvocationHandler`. Set
/// `CRATONVM_SYNTHETIC_ANNOTATIONS=1` or `CRATONVM_REAL_ANNOTATIONS=0` to retain
/// the old bare-`AnnotationProxy` representation for debugging.
pub fn real_annotations_enabled() -> bool {
    !crate::nbflags().synthetic_annotations && crate::nbflags().real_annotations
}

/// Wrap a synthetic `AnnotationProxy` data object (`handler`) in a real
/// `$ProxyN` proxy of the annotation interface `ann_cid`. Returns `None` (so the
/// caller falls back to the bare AnnotationProxy) if proxy-class generation
/// fails. The generated proxy's field layout mirrors `Proxy$Instance`:
/// field 0 = InvocationHandler, field 1 = `Class[]` interfaces, field 2 = id-hash.
pub fn wrap_annotation_in_real_proxy(
    ctx: &mut dyn NativeContext,
    ann_cid: ClassId,
    handler: ObjectRef,
) -> Option<ObjectRef> {
    // Class definition and every object/array allocation below can collect.
    // Keep the synthetic handler rooted until the generated proxy publishes it.
    let handler_pin = ctx.pin_native_root(handler);

    // Cache under the SAME namespace `Proxy.newProxyInstance(annotationType
    // .getClassLoader(), [annotationType], handler)` would use вЂ” this is
    // exactly the call Spring's `synthesize()` makes. A hardcoded `0` here
    // put every real-annotation proxy in the bootstrap-loader bucket
    // regardless of the annotation's actual defining loader, so a
    // `synthesize()`-built proxy for a non-bootstrap-loaded annotation type
    // landed in a different cache bucket than this one and never compared
    // `getClass()`-equal, even once dispatch returns correct hashCode/equals.
    // Mirrors `native_proxy_new_instance`'s `proxy_loader_namespace()` use.
    let ann_mirror = ctx.get_class_mirror(ann_cid);
    let ann_mirror_pin = ctx.pin_native_root(ann_mirror);
    let ann_mirror_cur = ctx.read_native_pin(ann_mirror_pin, ann_mirror);
    let annotation_loader: Option<ObjectRef> =
        match native_class_get_class_loader(ctx, &[Value::Object(Some(ann_mirror_cur))]) {
            Ok(Some(Value::Object(Some(loader_obj)))) => Some(loader_obj),
            _ => None,
        };
    let annotation_loader_pin = annotation_loader.map(|loader_obj| ctx.pin_native_root(loader_obj));
    let loader_namespace: u32 = annotation_loader
        .zip(annotation_loader_pin)
        .map(|(loader_obj, pin)| {
            let loader_cur = ctx.read_native_pin(pin, loader_obj);
            crate::proxy_loader_namespace(ctx, loader_cur)
        })
        .unwrap_or(0);
    let proxy_cid = match crate::reflect_annotations::define_or_get_proxy_class(
        ctx,
        loader_namespace,
        &[ann_cid],
    ) {
        crate::ProxyClassOutcome::Real(cid) => cid,
        // Gate-off degrade or a generation failure: fall back to the bare
        // AnnotationProxy. This annotation path degrades gracefully and is
        // independently optional (see real_annotations_enabled) вЂ” it never throws,
        // even when the proxy STRICT mode is on.
        _ => {
            ctx.unpin_native_roots(handler_pin);
            return None;
        }
    };
    if crate::nbflags().dbg_annproxy_wrap {
        let ann_name = ctx.class_name_of_id(ann_cid).unwrap_or_default();
        eprintln!(
            "[DBG_WRAP] ann_cid={} ann_name={ann_name} loader_namespace={loader_namespace} proxy_cid={}",
            ann_cid.as_u32(),
            proxy_cid.as_u32()
        );
    }
    // Register the annotation type's defining loader as the generated proxy
    // class's defining loader too, mirroring `native_proxy_new_instance` вЂ”
    // otherwise `proxyClass.getClassLoader()` falls back to the app-loader
    // default (`defining_loader_for` returns None) even when the annotation
    // was loaded by a user-defined loader
    // (MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader).
    if let (Some(loader_obj), Some(pin)) = (annotation_loader, annotation_loader_pin) {
        let loader_cur = ctx.read_native_pin(pin, loader_obj);
        if crate::classloader::is_user_defined_loader(ctx, loader_cur) {
            crate::classloader::register_defining_loader(
                crate::classloader::loader_tables(ctx),
                proxy_cid.as_u32(),
                loader_cur,
            );
        }
    }
    // SBR wave 6 (lane tail): under `--real-jdk` the real `Proxy.isProxyClass`
    // asks `ProxyBuilder.reverseProxyCache`, which only the real builder
    // writes; record this VM-generated class there so an annotation proxy is
    // still a proxy class. No-op outside `--real-jdk`.
    crate::reflect_annotations::register_vm_proxy_in_reverse_cache(ctx, proxy_cid);
    // zt t12/rt: zt t11/df3's opt-in `addClass` upcall here
    // (`CRATONVM_ZGC_DEFINE_ADD_CLASS_ANNOTATION_PROXY`) was retired. On a real
    // JDK the annotation proxy is served by `AnnotationParser` through
    // `defineClass1`, which makes the upcall (t10 / t11 rows: `proxy-in=true`
    // with the switch off, no `door=proxy`).
    let n = ctx.class_num_total_fields(proxy_cid).max(3);
    let real = ctx.alloc_object(proxy_cid, n);
    let real_pin = ctx.pin_native_root(real);
    let real_cur = ctx.read_native_pin(real_pin, real);
    let handler_cur = ctx.read_native_pin(handler_pin, handler);
    ctx.set_field(real_cur, 0, Value::Object(Some(handler_cur)));
    let type_mirror = ctx.read_native_pin(ann_mirror_pin, ann_mirror);
    let iface_arr = ctx.new_ref_array(ClassId::new(0), 1);
    let iface_arr_pin = ctx.pin_native_root(iface_arr);
    let iface_arr_cur = ctx.read_native_pin(iface_arr_pin, iface_arr);
    let type_mirror_cur = ctx.read_native_pin(ann_mirror_pin, type_mirror);
    ctx.set_array_element(iface_arr_cur, 0, Value::Object(Some(type_mirror_cur)));
    let real_cur = ctx.read_native_pin(real_pin, real);
    let iface_arr_cur = ctx.read_native_pin(iface_arr_pin, iface_arr);
    ctx.set_field(real_cur, 1, Value::Object(Some(iface_arr_cur)));
    let real_cur = ctx.read_native_pin(real_pin, real);
    ctx.set_field(real_cur, 2, Value::Int(0));
    let real_cur = ctx.read_native_pin(real_pin, real);
    ctx.unpin_native_roots(handler_pin);
    Some(real_cur)
}

// ---------------------------------------------------------------------------
// CRATONVM_REAL_ANNOTATIONS "2nd call site" support: hashCode/equals/toString
// computed directly via `NativeContext`.
//
// `native_proxy_dispatch_invoke` (native-builtins/src/lib.rs) is reached when
// a real `$ProxyN`'s generated hashCode/equals/toString body (bytecode that
// calls `Proxy$Dispatch.invokeProxy`) runs вЂ” this happens for every call once
// the JIT has compiled that generated body, bypassing the interpreter's
// primary AnnotationProxy dispatch hook. That hook's logic lives in
// `annotation_proxy_hash_code` / `annotation_proxy_equals` /
// `annotation_proxy_to_string` / the `"getClass"` arm of
// `annotation_proxy_dispatch_impl` (`vm/src/vm/vm_exec.rs`), which operate on
// `&SharedVm` directly. `native-builtins` cannot call into `vm` (the crate
// dependency only goes the other way вЂ” `vm` depends on `native-builtins`),
// and threading a `NativeContext` into the `vm`-side functions would mean
// touching the primary proxy-dispatch hot path used by every proxy consumer
// in the VM вЂ” out of scope for this narrow fix. These are therefore
// self-contained re-implementations against `NativeContext`, kept minimal on
// purpose (the common member-value shapes: primitives, String, Class, nested
// AnnotationProxy, boxed wrappers, arrays, enum-like objects). Keep in sync
// with the vm_exec.rs originals if annotation `Object`-method semantics
// change.
//
// "Keep in sync" was not enough, and this is what it cost: on 2026-08-12 the
// `toString` pair had drifted three ways at once (member ordering, binary vs
// canonical type name, `value=` elision), so ONE CratonVM run printed TWO
// different strings for the SAME annotation — `@AnnTwin$Single("v")` from the
// vm_exec copy and `@AnnTwin$Single(value="v")` from this one, neither being
// HotSpot's `@AnnTwin.Single("v")`. Which copy answers is not something a
// caller controls: measured, the first `toString()` on a fresh proxy can take
// the vm_exec hook and every later one this route, and it is not a JIT
// question — `-Xint` still shows both.
//
// The pure, VM-context-free half of `toString` therefore no longer lives in
// two places: `render_annotation_to_string` (below, `pub`) does the assembly
// and vm_exec.rs calls it. `vm` depends on `native-builtins`, never the
// reverse, so that is the only direction available; the extraction and
// value-rendering halves genuinely cannot be shared, because the two callers
// hold different VM handles from different crates.
// ---------------------------------------------------------------------------

pub fn ctx_class_name_of(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    let cid = ctx.class_id_of_object(obj);
    ctx.class_name_of_id(cid).unwrap_or_default()
}

pub fn ctx_wrapper_class_to_primitive(class_name: &str) -> Option<&'static str> {
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

/// Java-spec `String.hashCode()` вЂ” `s[0]*31^(n-1) + ... + s[n-1]`.
pub fn ctx_java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for ch in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(ch as i32);
    }
    h
}

pub fn ctx_annotation_proxy_elements(
    ctx: &mut dyn NativeContext,
    proxy: ObjectRef,
) -> Result<Vec<(String, Value)>, MethodCallFailed> {
    let names_arr = match ctx.get_field(proxy, ANN_PROXY_ELEM_NAMES) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Vec::new()),
    };
    let values_arr = match ctx.get_field(proxy, ANN_PROXY_ELEM_VALUES) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Vec::new()),
    };
    let n = ctx.array_length(names_arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let name = match ctx.get_array_element(names_arr, i) {
            Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
            _ => continue,
        };
        let val = ctx.get_array_element(values_arr, i);
        out.push((name, val));
    }
    Ok(out)
}

pub fn ctx_annotation_value_hash(
    ctx: &mut dyn NativeContext,
    val: Value,
) -> Result<i32, MethodCallFailed> {
    match val {
        Value::Int(i) => Ok(i),
        Value::Long(l) => Ok((l ^ (l >> 32)) as i32),
        Value::Float(f) => Ok(f.to_bits() as i32),
        Value::Double(d) => {
            let bits = d.to_bits() as i64;
            Ok((bits ^ (bits >> 32)) as i32)
        }
        Value::Object(None) | Value::Uninitialized => Ok(0),
        Value::Object(Some(obj)) => {
            if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                return Ok(ctx_annotation_array_hash(ctx, obj)?);
            }
            let cname = ctx_class_name_of(ctx, obj);
            if ctx_wrapper_class_to_primitive(&cname).is_some() {
                let inner = ctx.get_field(obj, 0);
                return ctx_annotation_value_hash(ctx, inner);
            }
            if cname == "java/lang/String" {
                if let Some(s) = ctx.read_string(obj) {
                    return Ok(ctx_java_string_hash(&s));
                }
            }
            if cname == "java/lang/annotation/AnnotationProxy" {
                return Ok(ctx_annotation_proxy_hash_code(ctx, obj)?);
            }
            // Class mirror, Enum constant, and any other reference-typed
            // member hash via `value.hashCode()` вЂ” identity hash, matching
            // `annotation_value_hash` in vm_exec.rs (Class/Enum do not
            // override `Object.hashCode()`).
            Ok(ctx.identity_hash_code(obj))
        }
        _ => Ok(0),
    }
}

pub fn ctx_annotation_array_hash(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let n = ctx.array_length(arr);
    let mut h: i32 = 1;
    for i in 0..n {
        let elem = ctx.get_array_element(arr, i);
        let elem_hash = ctx_annotation_value_hash(ctx, elem)?;
        h = h.wrapping_mul(31).wrapping_add(elem_hash);
    }
    Ok(h)
}

pub fn ctx_annotation_member_hash(
    ctx: &mut dyn NativeContext,
    name: &str,
    val: Value,
) -> Result<i32, MethodCallFailed> {
    let name_hash = ctx_java_string_hash(name);
    let value_hash = ctx_annotation_value_hash(ctx, val)?;
    Ok(127i32.wrapping_mul(name_hash) ^ value_hash)
}

/// Mirrors `annotation_proxy_hash_code` in vm_exec.rs.
pub fn ctx_annotation_proxy_hash_code(
    ctx: &mut dyn NativeContext,
    proxy: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    if annotation_jdk_semantics() {
        return jdk_annotation_hash(ctx, proxy);
    }
    let elems = ctx_annotation_proxy_elements(ctx, proxy)?;
    let mut h: i32 = 0;
    for (name, val) in elems {
        h = h.wrapping_add(ctx_annotation_member_hash(ctx, &name, val)?);
    }
    Ok(h)
}

pub fn ctx_annotation_values_equal(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x == y),
        (Value::Long(x), Value::Long(y)) => Ok(x == y),
        (Value::Float(x), Value::Float(y)) => Ok(x.to_bits() == y.to_bits()),
        (Value::Double(x), Value::Double(y)) => Ok(x.to_bits() == y.to_bits()),
        (Value::Object(None), Value::Object(None)) => Ok(true),
        (Value::Object(None), _) | (_, Value::Object(None)) => Ok(false),
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return Ok(true);
            }
            let xk = ctx.heap_kind_of(x);
            let yk = ctx.heap_kind_of(y);
            if xk == cratonvm_types::ObjectKind::Array || yk == cratonvm_types::ObjectKind::Array {
                if xk != yk {
                    return Ok(false);
                }
                let nx = ctx.array_length(x);
                let ny = ctx.array_length(y);
                if nx != ny {
                    return Ok(false);
                }
                for i in 0..nx {
                    let av = ctx.get_array_element(x, i);
                    let bv = ctx.get_array_element(y, i);
                    if !ctx_annotation_values_equal(ctx, av, bv)? {
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            let xname = ctx_class_name_of(ctx, x);
            let yname = ctx_class_name_of(ctx, y);
            if xname == "java/lang/annotation/AnnotationProxy"
                && yname == "java/lang/annotation/AnnotationProxy"
            {
                return Ok(ctx_annotation_proxy_equals(ctx, x, Value::Object(Some(y)))?);
            }
            if xname == "java/lang/String" && yname == "java/lang/String" {
                let sx = ctx.read_string(x).unwrap_or_default();
                let sy = ctx.read_string(y).unwrap_or_default();
                return Ok(sx == sy);
            }
            if xname == "java/lang/Class" && yname == "java/lang/Class" {
                // Identity, not slot 0. `mirror_class_id` asks
                // `class_id_from_mirror` FIRST and falls back to slot 0 only if
                // that misses, so this works in synthetic mode and keeps working
                // in real-JDK mode, where the slot-0 `ClassId` overlay is no
                // longer written (`vm/src/vm/vm_object.rs`, 2026-08-12 — slot 0
                // of a real `java.lang.Class` is `Constructor cachedConstructor`,
                // a declared REFERENCE field, and writing an `Int` there made
                // every collector box it).
                //
                // This site was the ONLY Class-mirror reader with no reverse-map
                // path — the 2026-08-10 sweep that fixed the others grepped the
                // token `mirror` and this function's variables are `x`/`y`. So
                // it was already wrong wherever the overlay was absent, and
                // would have become wrong everywhere.
                //
                // Two unresolvable mirrors must NOT compare equal merely by both
                // answering `None`; fall back to reference identity.
                return Ok(match (mirror_class_id(ctx, x), mirror_class_id(ctx, y)) {
                    (Some(a), Some(b)) => a == b,
                    _ => x.as_ptr() == y.as_ptr(),
                });
            }
            if ctx_wrapper_class_to_primitive(&xname).is_some()
                && ctx_wrapper_class_to_primitive(&yname).is_some()
            {
                let xv = ctx.get_field(x, 0);
                let yv = ctx.get_field(y, 0);
                return ctx_annotation_values_equal(ctx, xv, yv);
            }
            // Enum / generic вЂ” compare name field if present.
            if let (Value::Object(Some(xn)), Value::Object(Some(yn))) =
                (ctx.get_field(x, 0), ctx.get_field(y, 0))
            {
                let sx = ctx.read_string(xn).unwrap_or_default();
                let sy = ctx.read_string(yn).unwrap_or_default();
                if !sx.is_empty() && !sy.is_empty() {
                    return Ok(sx == sy && xname == yname);
                }
            }
            Ok(x == y)
        }
        _ => Ok(false),
    }
}

/// Mirrors `annotation_proxy_equals` in vm_exec.rs вЂ” the plain same-type
/// structural comparison `annotation_proxy_dispatch_impl`'s `"equals"` arm
/// uses. Does NOT implement the cross-type delegation to a foreign proxy's
/// own `equals` that `annotation_proxy_invoke_shared` layers on top (that
/// logic lives one level up from `annotation_proxy_dispatch_impl` and is out
/// of scope here); a foreign non-`AnnotationProxy` argument simply compares
/// unequal, same as calling this before that delegation was added.
pub fn ctx_annotation_proxy_equals(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    let other = match b {
        Value::Object(Some(o)) => o,
        _ => return Ok(false),
    };
    if a == other {
        return Ok(true);
    }
    if annotation_jdk_semantics() {
        // A nested or argument annotation is usually the generated `$ProxyN`
        // around a carrier, not the carrier itself.
        return match annotation_carrier_of(ctx, other) {
            Some(carrier) => jdk_annotation_equals(ctx, a, carrier),
            None => Ok(false),
        };
    }
    if ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object {
        return Ok(false);
    }
    if ctx_class_name_of(ctx, other) != "java/lang/annotation/AnnotationProxy" {
        return Ok(false);
    }
    let a_desc = match ctx.get_field(a, ANN_PROXY_TYPE_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let b_desc = match ctx.get_field(other, ANN_PROXY_TYPE_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if a_desc != b_desc {
        return Ok(false);
    }
    let a_elems = ctx_annotation_proxy_elements(ctx, a)?;
    let b_elems = ctx_annotation_proxy_elements(ctx, other)?;
    if a_elems.len() != b_elems.len() {
        return Ok(false);
    }
    for (name, av) in &a_elems {
        let bv = match b_elems.iter().find(|(n, _)| n == name) {
            Some((_, v)) => *v,
            None => return Ok(false),
        };
        if !ctx_annotation_values_equal(ctx, *av, bv)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn ctx_java_string_escape(s: &str) -> String {
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

pub fn ctx_format_annotation_value(
    ctx: &mut dyn NativeContext,
    val: Value,
) -> Result<String, MethodCallFailed> {
    match val {
        Value::Object(None) => Ok("null".to_string()),
        Value::Int(i) => Ok(i.to_string()),
        Value::Long(l) => Ok(l.to_string()),
        Value::Float(f) => Ok(f.to_string()),
        Value::Double(d) => Ok(d.to_string()),
        Value::Object(Some(obj)) => {
            if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                return Ok(ctx_format_annotation_array(ctx, obj)?);
            }
            let cname = ctx_class_name_of(ctx, obj);
            if cname == "java/lang/annotation/AnnotationProxy" {
                return Ok(ctx_annotation_proxy_to_string(ctx, obj)?);
            }
            if cname == "java/lang/String" {
                if let Some(s) = ctx.read_string(obj) {
                    return Ok(format!("\"{}\"", ctx_java_string_escape(&s)));
                }
            }
            if cname == "java/lang/Class" {
                // HotSpot's `AnnotationInvocationHandler.memberValueToString`
                // renders a `Class`-valued member as `getCanonicalName() +
                // ".class"`, so a member type prints `Outer.Inner.class` — not
                // the binary `Outer$Inner.class` a blind `$` rewrite yields, and
                // not `Dollar$Ann.class` turned into `Dollar.Ann.class`. Same
                // reasoning, same helper, and the same fallbacks as
                // `ctx_annotation_type_canonical_name` above; see its doc.
                if let Some(class_id) = ctx.class_id_from_mirror(obj) {
                    if let Some(slashed) = ctx.class_name_of_id(class_id) {
                        let canonical = canonical_class_name(ctx, class_id, &slashed);
                        if !canonical.is_empty() {
                            return Ok(format!("{}.class", &*canonical));
                        }
                        return Ok(format!("{}.class", slashed.replace('/', ".")));
                    }
                }
                // No class id behind the mirror (primitive and synthetic
                // mirrors among others): fall back to the mirror's name field.
                if let Value::Object(Some(name_ref)) = ctx.get_field(obj, 1) {
                    if let Some(cls_name) = ctx.read_string(name_ref) {
                        return Ok(format!("{}.class", cls_name.replace('/', ".")));
                    }
                }
                return Ok("<unknown>.class".to_string());
            }
            if let Some(prim_name) = ctx_wrapper_class_to_primitive(&cname) {
                let inner = ctx.get_field(obj, 0);
                let mut s = ctx_format_annotation_value(ctx, inner)?;
                if prim_name == "long" {
                    s.push('L');
                } else if prim_name == "float" {
                    s.push('f');
                }
                return Ok(s);
            }
            if let Value::Object(Some(name_ref)) = ctx.get_field(obj, 0) {
                if let Some(name) = ctx.read_string(name_ref) {
                    if !name.is_empty() {
                        return Ok(name);
                    }
                }
            }
            Ok(cname.replace('/', "."))
        }
        Value::Uninitialized => Ok("null".to_string()),
        _ => Ok("null".to_string()),
    }
}

pub fn ctx_format_annotation_array(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
) -> Result<String, MethodCallFailed> {
    let n = ctx.array_length(arr);
    let mut s = String::from("[");
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        let elem = ctx.get_array_element(arr, i);
        s.push_str(&ctx_format_annotation_value(ctx, elem)?);
    }
    s.push(']');
    Ok(s)
}

/// Assemble an `Annotation.toString()` from an ALREADY-canonicalised annotation
/// type name and its members with their values ALREADY rendered, in class-file
/// `element_value_pairs` order.
///
/// **This is the shared half of a rule that is implemented twice.** The other
/// implementation is `annotation_proxy_to_string` in `vm/src/vm/vm_exec.rs`
/// (the interpreter's primary `AnnotationProxy` dispatch hook); this file's
/// [`ctx_annotation_proxy_to_string`] is the `NativeContext` re-implementation
/// reached from `native_proxy_dispatch_invoke`. On 2026-08-12 the two disagreed
/// in a single run — the same annotation rendered `@AnnTwin$Single("v")` from
/// one and `@AnnTwin$Single(value="v")` from the other — so the assembly step,
/// which is pure and needs no VM context at all, now lives here and both sides
/// call it. `vm` depends on `native-builtins` (never the reverse), so this
/// direction is the only one available.
///
/// What is NOT shared, and why: extracting the members, canonicalising the type
/// name, and rendering each value all need a VM handle, and the two callers hold
/// different ones (`&SharedVm` vs `&mut dyn NativeContext`) from different
/// crates. Those steps are still duplicated; keep them in sync deliberately.
///
/// Two rules are baked in here:
///
/// * **Order is the caller's, and is NOT sorted.** HotSpot's
///   `AnnotationInvocationHandler` iterates the `LinkedHashMap` that
///   `AnnotationParser.parseAnnotation2` fills in class-file order, so the
///   class file's `element_value_pairs` order is what prints.
/// * **`value=` is elided for a single-member annotation whose sole member is
///   named `value`** — `@Qualifier("alpha")`, not `@Qualifier(value="alpha")`.
///   Any multi-member annotation keeps every name, `value` included.
pub fn render_annotation_to_string(
    canonical_type_name: &str,
    members: &[(String, String)],
) -> String {
    let mut s = String::with_capacity(32 + canonical_type_name.len());
    s.push('@');
    s.push_str(canonical_type_name);
    s.push('(');
    let omit_single_value_name = members.len() == 1 && members[0].0 == "value";
    for (i, (name, rendered)) in members.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        if !omit_single_value_name {
            s.push_str(name);
            s.push('=');
        }
        s.push_str(rendered);
    }
    s.push(')');
    s
}

/// The annotation type's CANONICAL name (JLS 6.7) — what HotSpot's
/// `AnnotationInvocationHandler.toString()` prints. It formats
/// `annotationType().getCanonicalName()`, not `getName()`, so a member
/// annotation type renders with `.` where the binary name has `$`. Measured on
/// Microsoft OpenJDK 25.0.3.9: `@AnnA40.Multi(...)` for the type whose binary
/// name is `AnnA40$Multi`, and `@AnnA40.Nest.Deep(...)` two levels down.
///
/// Resolved through [`canonical_class_name`] — i.e. the annotation type's own
/// `InnerClasses` attribute (JVMS §4.7.6) — and **not** by rewriting every `$`
/// in the binary name: `$` is a legal Java identifier character, so a top-level
/// `@interface Dollar$Ann` has canonical name `Dollar$Ann` and a member type may
/// legally be named `Inner$Class`. That is the distinction
/// `native_class_get_canonical_name` / `own_inner_class_entry` already draw, and
/// which `class_get_canonical_name_preserves_literal_dollar_in_member_name`
/// pins; going through the same helper also shares its per-`ClassId` cache.
///
/// [`canonical_class_name`]'s empty-string sentinel means "no canonical name"
/// (local/anonymous, JLS 6.7). JLS 9.6 admits only top-level and member
/// annotation types, so that branch is unreachable for a real annotation type;
/// it is still handled, falling back to the dotted binary name rather than
/// rendering a `null` this caller has no form for. Every other miss (no mirror,
/// no class id) takes the same fallback, which is exactly the string this
/// function's caller used to produce unconditionally.
pub fn ctx_annotation_type_canonical_name(
    ctx: &mut dyn NativeContext,
    proxy: ObjectRef,
    internal: &str,
) -> String {
    let dotted_binary = internal.replace('/', ".");
    // Slot 1 is `ANN_PROXY_TYPE_MIRROR`; `create_annotation_proxy_with_type`
    // falls back to the admitted `ClassId` precisely so callers may dereference
    // it without a null check, so a miss means a proxy minted by some other
    // route and the binary name is the honest answer for it.
    let mirror = match ctx.get_field(proxy, ANN_PROXY_TYPE_MIRROR) {
        Value::Object(Some(m)) => m,
        _ => return dotted_binary,
    };
    let class_id = match ctx.class_id_from_mirror(mirror) {
        Some(id) => id,
        None => return dotted_binary,
    };
    // Pass the class's OWN name, not the descriptor-derived one: the canonical
    // cache is keyed by `ClassId` and this argument only feeds a cache MISS, so
    // handing it a name that disagreed with the class would poison the entry
    // `Class.getCanonicalName()` reads.
    let slashed = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return dotted_binary,
    };
    let canonical = canonical_class_name(ctx, class_id, &slashed);
    if canonical.is_empty() {
        dotted_binary
    } else {
        (*canonical).to_string()
    }
}

/// Mirrors `annotation_proxy_to_string` in `vm/src/vm/vm_exec.rs`. The two must
/// render identically — see [`render_annotation_to_string`], which is the part
/// of the rule they share, for why there are two of these at all.
pub fn ctx_annotation_proxy_to_string(
    ctx: &mut dyn NativeContext,
    proxy: ObjectRef,
) -> Result<String, MethodCallFailed> {
    let desc = match ctx.get_field(proxy, ANN_PROXY_TYPE_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let class_name =
        if let Some(stripped) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            stripped.to_string()
        } else if desc.is_empty() {
            "<unknown>".to_string()
        } else {
            desc.clone()
        };
    let dotted = ctx_annotation_type_canonical_name(ctx, proxy, &class_name);
    if annotation_jdk_semantics() {
        let members = jdk_annotation_members_in_map_order(ctx, proxy)?;
        let mut rendered = Vec::with_capacity(members.len());
        for (name, val) in members {
            let text = jdk_member_value_to_string(ctx, val)?;
            rendered.push((name, text));
        }
        return Ok(render_annotation_to_string(&dotted, &rendered));
    }
    // NOT sorted: `ctx_annotation_proxy_elements` hands these back in class-file
    // `element_value_pairs` order, which is the order HotSpot prints.
    let elems = ctx_annotation_proxy_elements(ctx, proxy)?;
    let mut members = Vec::with_capacity(elems.len());
    for (name, val) in elems {
        let rendered = ctx_format_annotation_value(ctx, val)?;
        members.push((name, rendered));
    }
    Ok(render_annotation_to_string(&dotted, &members))
}

// ---------------------------------------------------------------------------
// Round 13 wave 6 (lane proxy4): `sun.reflect.annotation.AnnotationInvocationHandler`
// semantics for the VM's annotation carrier (`AnnotationProxy`), in both the
// generated-body door (`native_proxy_dispatch_invoke`) and the interpreter's
// (`vm_exec.rs` `annotation_proxy_invoke_shared`, through
// [`annotation_carrier_object_method`]), so ONE implementation answers
// `toString` / `hashCode` / `equals`. What the older twins got wrong, measured
// by `R13Proxy4AnnotationMembers` against HotSpot 25:
//
// * a nested annotation is a generated `$ProxyN` whose handler is the carrier,
//   not a carrier: `toString` printed `$Proxy0`, `hashCode` took its identity
//   hash and `equals` compared it by identity, so two equal annotations with a
//   nested member were unequal and hashed apart;
// * `toStringImpl`'s value forms: `true`, `'q'`, `(byte)0x04`, `2.5f`, `2L`,
//   `{..}` arrays -- the twins printed `1`, `113`, `4`, `[..]`;
// * `Boolean.hashCode` is 1231 / 1237 (scalar and `boolean[]` elements), not 1/0;
// * member order is `memberValues`' order: a `LinkedHashMap` seeded from
//   `AnnotationType.memberDefaults()`, a `new HashMap<>(0)`, so defaulted
//   members come first in that map's bucket order, then the members with no
//   default in class-file order.
//
// `CRATONVM_ANNOTATION_JDK_SEMANTICS=0` restores the previous answers (and, in
// `vm_exec.rs`, the old routing and no defensive copy of array members).
// ---------------------------------------------------------------------------

/// `CRATONVM_ANNOTATION_JDK_SEMANTICS` (default on, both modes). Read by this
/// file's annotation-carrier `toString` / `hashCode` / `equals` and by
/// `vm::vm::vm_exec`'s `annotation_proxy_invoke_shared`.
pub fn annotation_jdk_semantics() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The VM annotation carrier behind `obj`: `obj` itself when it is one, the
/// handler (slot 0) of a generated proxy whose handler is one, else `None`.
pub fn annotation_carrier_of(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return None;
    }
    if ctx_class_name_of(ctx, obj) == "java/lang/annotation/AnnotationProxy" {
        return Some(obj);
    }
    let cid = ctx.class_id_of_object(obj);
    if !crate::reflect_annotations::proxy_class_is_generated(&*ctx, cid) {
        return None;
    }
    match ctx.get_field(obj, 0) {
        Value::Object(Some(handler))
            if ctx_class_name_of(ctx, handler) == "java/lang/annotation/AnnotationProxy" =>
        {
            Some(handler)
        }
        _ => None,
    }
}

/// The table capacity of `new HashMap<>(0)` after `entries` puts
/// (`HashMap.resize`: a zero initial capacity starts at one bucket and
/// doubles; the threshold is `capacity * 0.75`, doubled with it from 16 on).
pub fn jdk_hashmap0_capacity(entries: usize) -> usize {
    fn resize(cap: usize, thr: usize) -> (usize, usize) {
        let (new_cap, mut new_thr) = if cap > 0 {
            (cap * 2, if cap >= 16 { thr * 2 } else { 0 })
        } else if thr > 0 {
            (thr, 0)
        } else {
            (16, 12)
        };
        if new_thr == 0 {
            new_thr = new_cap * 3 / 4;
        }
        (new_cap, new_thr)
    }
    // `tableSizeFor(0)` is 1: the initial threshold of `new HashMap<>(0)`.
    let (mut cap, mut thr) = (0usize, 1usize);
    for size in 1..=entries {
        if cap == 0 {
            (cap, thr) = resize(cap, thr);
        }
        if size > thr {
            (cap, thr) = resize(cap, thr);
        }
    }
    cap.max(1)
}

/// `HashMap`'s bucket for a `String` key in a table of `cap` (a power of two).
pub fn jdk_hashmap_bucket(name: &str, cap: usize) -> usize {
    let h = ctx_java_string_hash(name) as u32;
    ((h ^ (h >> 16)) as usize) & (cap - 1)
}

// ---------------------------------------------------------------------------
// Round 16 wave 13 (lane annorder): HotSpot's `getDeclaredMethods()` order for
// an annotation interface. It decides the order in which two defaulted members
// that share one `AnnotationType.memberDefaults()` bucket print in
// `Annotation.toString()` (the map is filled in `getDeclaredMethods()` order,
// and a bucket's chain keeps insertion order).
//
// HotSpot sorts a class's `methods` array by the ADDRESS of each method's name
// `Symbol` (`Method::sort_methods`, `Symbol::fast_compare`), and
// `JVM_GetClassDeclaredMethods` lists them in that order. In a default run the
// JDK's CDS archive (`lib/server/classes.jsa`; `bin/server/` on Windows) is
// mapped below every C-heap allocation, so a name the archive already holds is
// an archived `Symbol` and sorts before any name created at run time, and
// archived symbols sort by their position in the archive. Measured on Microsoft
// OpenJDK 25.0.3.9: `getDeclaredMethods()` of a 16-member annotation, and a
// battery of 120 random annotations (4-14 defaulted members each, ~70 % JDK
// names), print every archived name in archive-offset order, ahead of every
// run-time name. Only pairs of run-time names in one bucket can still differ
// (12 + 8 of 120): those follow malloc addresses (creation order in the
// common case, which is class-file order here), and `-Xshare:off` scrambles
// them between runs of the same JVM, so no rule reproduces them.
// ---------------------------------------------------------------------------

/// `CRATONVM_ANNOTATION_CDS_MEMBER_ORDER` (default on, both modes). On:
/// `Class.getDeclaredMethods()` of an annotation interface lists its methods
/// in HotSpot's order (archived names first, in CDS-archive order, then the
/// rest in class-file order), and the carrier's `toString` puts two defaulted
/// members of one `memberDefaults` bucket in that order. `=0`: class-file
/// order for `getDeclaredMethods()` and name order inside a bucket (the
/// previous answers).
pub fn annotation_cds_member_order() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// Method name -> file offset of that name's `Symbol` in the JDK's default
/// CDS archive. Empty when the archive is absent or unreadable, which makes
/// every name a "run-time" name and leaves class-file order in place.
#[derive(Default)]
pub struct CdsSymbolIndex {
    pub offsets: FxHashMap<u64, u32>,
}

impl CdsSymbolIndex {
    #[inline]
    pub fn hash_name(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Index the archived `Symbol`s of a `classes.jsa` image whose body is a
    /// Java identifier (the only names a method can have that matter here).
    ///
    /// A 64-bit `Symbol` is `u4 _hash_and_refcount; u2 _length; u1 _body[]`,
    /// 8-byte aligned, and every archived symbol is permanent: refcount
    /// `0xFFFF` in the low half of the first word (little-endian bytes
    /// `FF FF`). Only the first record of a name is kept (the archive holds one
    /// `Symbol` per name; a later look-alike would be data, not a symbol).
    pub fn from_archive_bytes(bytes: &[u8]) -> Self {
        let mut offsets: FxHashMap<u64, u32> =
            FxHashMap::with_capacity_and_hasher(bytes.len() / 512, Default::default());
        let mut o = 0usize;
        while o + 6 <= bytes.len() {
            if bytes[o] == 0xFF && bytes[o + 1] == 0xFF {
                let len = u16::from_le_bytes([bytes[o + 4], bytes[o + 5]]) as usize;
                let body_end = o + 6 + len;
                if (1..=255).contains(&len) && body_end <= bytes.len() {
                    let body = &bytes[o + 6..body_end];
                    // `<init>` / `<clinit>` too (interpreter round i4 wave
                    // 98, lane L3: `hotspot_method_array_order` ranks them).
                    let ident = (!body[0].is_ascii_digit()
                        && body
                            .iter()
                            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$'))
                        || body == b"<init>"
                        || body == b"<clinit>";
                    if ident {
                        if let Ok(off) = u32::try_from(o) {
                            offsets.entry(Self::hash_name(body)).or_insert(off);
                        }
                    }
                }
            }
            o += 8;
        }
        Self { offsets }
    }

    pub fn offset_of(&self, name: &str) -> Option<u32> {
        self.offsets.get(&Self::hash_name(name.as_bytes())).copied()
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn load(java_home: Option<String>) -> Self {
        let Some(home) = java_home.filter(|h| !h.is_empty()) else {
            return Self::default();
        };
        for rel in ["lib/server/classes.jsa", "bin/server/classes.jsa"] {
            let path = std::path::Path::new(&home).join(rel);
            if let Ok(file) = std::fs::File::open(&path) {
                if let Ok(mmap) = unsafe { memmap2::Mmap::map(&file) } {
                    return Self::from_archive_bytes(&mmap);
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    return Self::from_archive_bytes(&bytes);
                }
            }
        }
        Self::default()
    }
}

/// Per-VM [`CdsSymbolIndex`] of the VM's `java.home`, built on first use
/// (one read of the ~16 MB archive; ~21 000 names). Dropped with the VM by
/// [`forget_vm_annotation_proxies`].
pub static CDS_SYMBOL_INDEX: VmScoped<Option<Arc<CdsSymbolIndex>>> = VmScoped::new();

pub fn load_cds_symbol_index_for_home(java_home: Option<String>) -> Arc<CdsSymbolIndex> {
    let mut built = CdsSymbolIndex::load(java_home.clone());
    if built.len() == 0 {
        for var in ["CRATONVM_JAVA_HOME", "JAVA_HOME"] {
            if let Ok(home) = cratonvm_types::flags::runtime_var(var) {
                built = CdsSymbolIndex::load(Some(home));
                if built.len() > 0 {
                    break;
                }
            }
        }
    }
    if annotation_order_dbg() {
        eprintln!(
            "[gdmorder] cds index built: java.home={:?} names={}",
            java_home,
            built.len()
        );
    }
    Arc::new(built)
}

pub fn prewarm_cds_symbol_index(vm_identity: usize, java_home: Option<String>) {
    if matches!(CDS_SYMBOL_INDEX.peek(vm_identity, |row| row.is_some()), Some(true)) {
        return;
    }
    let built = load_cds_symbol_index_for_home(java_home.clone());
    let home_unset = !matches!(java_home.as_deref(), Some(h) if !h.is_empty() && h != ".");
    if built.len() == 0 && home_unset {
        return;
    }
    CDS_SYMBOL_INDEX.with(vm_identity, |row| {
        let _ = row.get_or_insert(built);
    });
}

pub fn cds_symbol_index(ctx: &mut dyn NativeContext) -> Arc<CdsSymbolIndex> {
    let vm = ctx.vm_identity();
    if let Some(Some(index)) = CDS_SYMBOL_INDEX.peek(vm, |row| row.clone()) {
        return index;
    }
    let java_home = ctx.get_system_property("java.home");
    let built = load_cds_symbol_index_for_home(java_home.clone());
    let home_unset = !matches!(java_home.as_deref(), Some(h) if !h.is_empty() && h != ".");
    if built.len() == 0 && home_unset {
        return built;
    }
    CDS_SYMBOL_INDEX.with(vm, |row| Arc::clone(row.get_or_insert(built)))
}

/// HotSpot's `getDeclaredMethods()` order of `names` (given in class-file
/// order), as a permutation of their indices: archived names by archive
/// offset, then the others in class-file order. Stable, so overloads (one
/// `Symbol` on HotSpot) keep their class-file order.
pub fn hotspot_declared_method_order(index: &CdsSymbolIndex, names: &[&str]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by_key(|&i| match index.offset_of(names[i]) {
        Some(off) => (0u8, off as usize),
        None => (1u8, i),
    });
    order
}

/// May `getDeclaredMethods()` of the non-annotation class `class_id`, whose
/// methods are `methods` (class-file order), take HotSpot's order
/// ([`hotspot_method_array_order`])? JIT round 21 (lane npecompat, AO-1 of
/// `jit-r16-annorder-proposals-RETIRED-20261006.md`): only where that order can
/// be stated exactly, which is when every key of HotSpot's sort is known:
///
/// * every method name (`<init>` / `<clinit>` included: they are in the
///   sorted array too, and the quicksort is not stable, so where they sort
///   moves the overloads around them) is a `Symbol` of the JDK's CDS archive
///   (`CdsSymbolIndex`, non-empty). Their addresses are then archive base +
///   offset, whatever base the archive is mapped (or relocated) at, so their
///   order is the offsets'. A name created at run time has a malloc address:
///   where it sorts against the archive, and against another run-time name,
///   depends on the allocator and on the archive's mapping, and is not
///   reproduced -- such a class keeps the class-file / bridge-adjacency order
///   (as before). A lambda body (`lambda$main$0`), most application names,
///   and every synthetic accessor name fall there.
/// * the class was sorted ONCE, at parse time, from class-file order: an
///   application class (a definite application or user loader). A class of
///   the runtime image may come from the archive, whose method arrays were
///   re-sorted at dump time from another input order (same answer for
///   distinct names, not for overloads); those keep the current order.
/// * not a lambda proxy (VM-synthesized members, `declared_methods_with_synthetic`).
pub fn plain_class_method_order_is_exact(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    methods: &[MethodMetadata],
) -> bool {
    if is_lambda_proxy_id(class_id)
        || !ctx
            .loader_id_of_class_definite(class_id)
            .is_some_and(|loader| loader >= 2)
        || ctx.class_access_flags(class_id) & 0x2000 != 0
    {
        return false;
    }
    let index = cds_symbol_index(ctx);
    index.len() > 0 && methods.iter().all(|m| index.offset_of(&m.name).is_some())
}

/// HotSpot's `InstanceKlass::methods()` order of a class whose methods are
/// `names` (class-file order), as a permutation of their indices: the result
/// of HotSpot's own `Method::sort_methods`, i.e. `QuickSort::sort(methods,
/// method_comparator, /*idempotent=*/false)` keyed by the name `Symbol`'s
/// address. The address is modelled as [`hotspot_declared_method_order`]
/// does (archived names by archive offset, ahead of run-time names, which
/// follow their first appearance in the class file).
///
/// Unlike a stable sort this reproduces the order HotSpot's quicksort leaves
/// OVERLOADS in (one `Symbol`, equal keys), which `JVM_GetClassDeclared
/// Constructors` lists as-is. Interpreter round i4 wave 98, lane L3
/// (`L3W98ReflectAnnotations` row `En ctor`: HotSpot lists an enum's
/// `<init>(String,int)` before its `<init>(String,int,String)` although the
/// class file has them the other way round).
pub fn hotspot_method_array_order(index: &CdsSymbolIndex, names: &[&str]) -> Vec<usize> {
    hotspot_method_array_order_ranked(index, names, &FxHashMap::default())
}

/// [`hotspot_method_array_order`] with a better model of a run-time name's
/// `Symbol` address (interpreter round i5 wave 101, lane L3,
/// `CRATONVM_REFLECT_HOTSPOT_METHOD_ORDER`): `pool_rank` maps a name the
/// archive does not hold to the index of its `CONSTANT_Utf8` entry in the
/// class's constant pool. HotSpot's `ClassFileParser` creates a class's new
/// symbols in constant-pool order (`SymbolTable::new_symbols`), so of two
/// names first created by this class, the one earlier in the pool has the
/// lower address -- which is not the method-table order whenever an earlier
/// method's code names a later method. Archived names still rank first (by
/// archive offset), then pool-ranked names, then names in neither (a
/// synthesized member) by first appearance. A name some earlier class already
/// created keeps that older address on HotSpot; that part is not modelled.
pub fn hotspot_method_array_order_ranked(
    index: &CdsSymbolIndex,
    names: &[&str],
    pool_rank: &FxHashMap<String, usize>,
) -> Vec<usize> {
    let mut first_seen: FxHashMap<&str, usize> = FxHashMap::default();
    for (i, n) in names.iter().enumerate() {
        first_seen.entry(*n).or_insert(i);
    }
    let rank: Vec<(u8, usize)> = names
        .iter()
        .map(|n| match index.offset_of(n) {
            Some(off) => (0u8, off as usize),
            None => match pool_rank.get(*n) {
                Some(&cp) => (1u8, cp),
                None => (2u8, first_seen[n]),
            },
        })
        .collect();
    let mut order: Vec<usize> = (0..names.len()).collect();
    hotspot_quicksort(&mut order, &|a, b| rank[a].cmp(&rank[b]));
    order
}

/// The constant-pool index of the `CONSTANT_Utf8` entry of each of `names`
/// that `index` does not hold, for [`hotspot_method_array_order_ranked`].
/// Empty for a class with no pool here (a synthesized class).
pub fn method_name_pool_ranks(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    index: &CdsSymbolIndex,
    names: &[&str],
) -> FxHashMap<String, usize> {
    let mut wanted: FxHashMap<Vec<u16>, &str> = FxHashMap::default();
    for n in names {
        if index.offset_of(n).is_none() {
            wanted.entry(n.encode_utf16().collect()).or_insert(*n);
        }
    }
    let mut out: FxHashMap<String, usize> = FxHashMap::default();
    if wanted.is_empty() {
        return out;
    }
    let size = ctx.reflect_constant_pool_size(class_id).unwrap_or(0);
    for i in 1..size {
        let Ok(at) = i32::try_from(i) else {
            break;
        };
        if let cratonvm_native_api::ReflectConstant::Utf8(units) =
            ctx.reflect_constant_pool_entry(class_id, at)
        {
            if let Some(name) = wanted.remove(&units) {
                out.insert(name.to_string(), i);
                if wanted.is_empty() {
                    break;
                }
            }
        }
    }
    out
}

/// HotSpot's `QuickSort::inner_sort<false>` (`utilities/quickSort.hpp`):
/// median-of-three pivot (which alone sorts up to three elements), then a
/// Hoare partition that swaps equal elements too, recursing on both halves.
pub fn hotspot_quicksort(a: &mut [usize], cmp: &dyn Fn(usize, usize) -> std::cmp::Ordering) {
    use std::cmp::Ordering::{Greater, Less};
    let len = a.len();
    if len < 2 {
        return;
    }
    let middle = len / 2;
    let last = len - 1;
    if cmp(a[0], a[middle]) == Greater {
        a.swap(0, middle);
    }
    if cmp(a[0], a[last]) == Greater {
        a.swap(0, last);
    }
    if cmp(a[middle], a[last]) == Greater {
        a.swap(middle, last);
    }
    if len < 4 {
        return;
    }
    let pivot = a[middle];
    let (mut left, mut right) = (0usize, last);
    let split = loop {
        while cmp(a[left], pivot) == Less {
            left += 1;
        }
        while cmp(a[right], pivot) == Greater {
            right -= 1;
        }
        if left < right {
            a.swap(left, right);
            left += 1;
            right -= 1;
        } else {
            break right;
        }
    };
    let (lo, hi) = a.split_at_mut(split + 1);
    hotspot_quicksort(lo, cmp);
    hotspot_quicksort(hi, cmp);
}

/// Whether two of `names` fall in one bucket of `memberDefaults`.
pub fn member_defaults_share_a_bucket(names: &[String]) -> bool {
    let cap = jdk_hashmap0_capacity(names.len());
    let mut buckets: Vec<usize> = names.iter().map(|n| jdk_hashmap_bucket(n, cap)).collect();
    buckets.sort_unstable();
    buckets.windows(2).any(|w| w[0] == w[1])
}

/// `new LinkedHashMap<>(memberDefaults)`'s key order, given the defaulted
/// member names in the order `AnnotationType` put them (its
/// `getDeclaredMethods()` order): bucket by bucket, each bucket's chain in
/// insertion order (`HashMap.resize` splits keep it). A stable sort by bucket.
pub fn member_defaults_iteration_order(mut put_order: Vec<String>) -> Vec<String> {
    let cap = jdk_hashmap0_capacity(put_order.len());
    put_order.sort_by_key(|n| jdk_hashmap_bucket(n, cap));
    put_order
}

/// The carrier's members in `AnnotationInvocationHandler.memberValues` order:
/// the members that have an `AnnotationDefault` first, in the bucket order of
/// `AnnotationType.memberDefaults()` (a `new HashMap<>(0)`), then the members
/// without one in class-file (`element_value_pairs`) order. Two defaulted
/// members in ONE bucket keep `getDeclaredMethods()` order on HotSpot, which
/// follows the method-name `Symbol` addresses; with
/// [`annotation_cds_member_order`] (default) that is reproduced from the JDK's
/// CDS archive ([`hotspot_declared_method_order`]; exact for every archived
/// name, class-file order for the rest), `=0` takes them in name order (round
/// 16 wave 9 lane proxy measured the in-bucket pairs of `R16ProxyAnnOrder`
/// reversed under that rule). Falls back to the carrier's own order when the
/// annotation type is not reachable.
pub fn jdk_annotation_members_in_map_order(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
) -> Result<Vec<(String, Value)>, MethodCallFailed> {
    let elems = ctx_annotation_proxy_elements(ctx, carrier)?;
    let mirror_cid = match ctx.get_field(carrier, ANN_PROXY_TYPE_MIRROR) {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    let (type_cid, type_via) = if annotation_cds_member_order() {
        annotation_type_declaring_members(ctx, carrier, mirror_cid, &elems)
    } else {
        (mirror_cid, "mirror")
    };
    let Some(type_cid) = type_cid else {
        if annotation_order_dbg() {
            eprintln!(
                "[annorder] type=<unreachable> via={type_via} members={:?} -> carrier order",
                elems.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
            );
        }
        return Ok(elems);
    };
    // Round 16 wave 16 (lane proxy6, proposal AO-2): the defaulted order is a
    // fact of the annotation TYPE, memoized per type with
    // `CRATONVM_ANNOTATION_MEMBER_ORDER_MEMO=1`.
    let memo = annotation_member_order_memo();
    let memoized = if memo {
        memoized_defaulted_order(ctx.vm_identity(), type_cid)
    } else {
        None
    };
    let defaulted: Vec<String> = match memoized {
        Some(order) => order.iter().map(|n| n.to_string()).collect(),
        None => {
            let (order, keep) = defaulted_members_in_map_order(ctx, type_cid, type_via);
            if memo && keep {
                remember_defaulted_order(ctx.vm_identity(), type_cid, &order);
            }
            order
        }
    };
    let mut out: Vec<(String, Value)> = Vec::with_capacity(elems.len());
    for name in &defaulted {
        if let Some((n, v)) = elems.iter().find(|(n, _)| n == name) {
            out.push((n.clone(), *v));
        }
    }
    for (n, v) in &elems {
        if !defaulted.iter().any(|d| d == n) {
            out.push((n.clone(), *v));
        }
    }
    if annotation_order_dbg() {
        eprintln!(
            "[annorder] order={:?}",
            out.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
        );
    }
    Ok(out)
}

/// The defaulted members of `type_cid` in `AnnotationType.memberDefaults()`
/// iteration order (the body of [`jdk_annotation_members_in_map_order`]),
/// and whether the answer may be memoized: not when two defaulted members
/// share a bucket and the CDS index came out empty (`cds_symbol_index` does
/// not keep such an index either; a later call may see `java.home`).
pub fn defaulted_members_in_map_order(
    ctx: &mut dyn NativeContext,
    type_cid: ClassId,
    type_via: &str,
) -> (Vec<String>, bool) {
    let mut keep = true;
    let methods = ctx.declared_methods(type_cid);
    let mut defaulted: Vec<String> = Vec::new();
    for m in &methods {
        if m.name.starts_with('<') || !m.descriptor.starts_with("()") {
            continue;
        }
        if ctx
            .method_annotation_default(type_cid, &m.name, &m.descriptor)
            .is_some()
        {
            defaulted.push(m.name.clone());
        }
    }
    if annotation_cds_member_order() {
        // `defaulted` is in class-file order; `AnnotationType` puts them in
        // `getDeclaredMethods()` order, which only matters inside a bucket.
        let shared_bucket = member_defaults_share_a_bucket(&defaulted);
        if shared_bucket || annotation_order_dbg() {
            let index = cds_symbol_index(ctx);
            if shared_bucket && index.len() == 0 {
                keep = false;
            }
            if annotation_order_dbg() {
                eprintln!(
                    "[annorder] type={} via={type_via} declared={} defaulted={:?} \
                     shared_bucket={shared_bucket} index_names={} offsets={:?}",
                    ctx.class_name_of_id(type_cid).unwrap_or_default(),
                    methods.len(),
                    defaulted,
                    index.len(),
                    defaulted
                        .iter()
                        .map(|n| index.offset_of(n))
                        .collect::<Vec<_>>()
                );
            }
            if shared_bucket {
                let names: Vec<&str> = defaulted.iter().map(String::as_str).collect();
                let order = hotspot_declared_method_order(&index, &names);
                defaulted = order.into_iter().map(|i| defaulted[i].clone()).collect();
            }
        }
        defaulted = member_defaults_iteration_order(defaulted);
    } else {
        let cap = jdk_hashmap0_capacity(defaulted.len());
        defaulted.sort_by(|a, b| {
            (jdk_hashmap_bucket(a, cap), a.as_str()).cmp(&(jdk_hashmap_bucket(b, cap), b.as_str()))
        });
    }
    (defaulted, keep)
}

// ---------------------------------------------------------------------------
// Round 16 wave 16 (lane proxy6), proposal AO-2 of
// `docs/feature-designs/jit-ir-optimizations.md`: the `--compatible`
// carrier's `toString` recomputed, on every call, the annotation type's
// declared methods (once or twice for the type check), each member's
// `AnnotationDefault` and the bucket sort. HotSpot computes `AnnotationType`
// once per class. With `CRATONVM_ANNOTATION_MEMBER_ORDER_MEMO=1` (default off
// until measured) both facts are memoized per (VM, annotation type): a
// per-VM row, purged for unloaded or redefined classes
// (`forget_unloaded_annotation_proxies` / `forget_redefined_annotation_proxies`)
// and at VM teardown. The answers are unchanged.
// ---------------------------------------------------------------------------

/// `CRATONVM_ANNOTATION_MEMBER_ORDER_MEMO` (default ON since wave 17, `=0` turns it off;
/// w16a: `R16Proxy6AnnToString` 6637 -> 5371 ms): see above.
pub fn annotation_member_order_memo() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// One annotation type's memoized member facts.
#[derive(Clone, Default)]
pub struct MemberOrderRow {
    /// Every declared method name (`annotation_type_declaring_members`).
    pub declared: Option<Arc<[Box<str>]>>,
    /// The defaulted members in `memberDefaults` iteration order.
    pub defaulted: Option<Arc<[Box<str>]>>,
}

/// Per VM: annotation type class id -> its memoized member facts.
pub static ANNOTATION_MEMBER_ORDER_MEMO: VmScoped<FxHashMap<u32, MemberOrderRow>> = VmScoped::new();

pub fn memoized_defaulted_order(vm: usize, type_cid: ClassId) -> Option<Arc<[Box<str>]>> {
    ANNOTATION_MEMBER_ORDER_MEMO
        .peek(vm, |table| {
            table
                .get(&type_cid.as_u32())
                .and_then(|row| row.defaulted.clone())
        })
        .flatten()
}

pub fn remember_defaulted_order(vm: usize, type_cid: ClassId, order: &[String]) {
    let order: Arc<[Box<str>]> = order.iter().map(|n| n.clone().into_boxed_str()).collect();
    ANNOTATION_MEMBER_ORDER_MEMO.with(vm, |table| {
        table.entry(type_cid.as_u32()).or_default().defaulted = Some(order);
    });
}

/// Every method name `cid` declares, memoized with the switch on.
pub fn annotation_declared_method_names(ctx: &mut dyn NativeContext, cid: ClassId) -> Arc<[Box<str>]> {
    let memo = annotation_member_order_memo();
    let vm = ctx.vm_identity();
    if memo {
        let hit = ANNOTATION_MEMBER_ORDER_MEMO
            .peek(vm, |table| {
                table
                    .get(&cid.as_u32())
                    .and_then(|row| row.declared.clone())
            })
            .flatten();
        if let Some(names) = hit {
            return names;
        }
    }
    let names: Arc<[Box<str>]> = ctx
        .declared_methods(cid)
        .into_iter()
        .map(|m| m.name.into_boxed_str())
        .collect();
    if memo {
        let row_names = Arc::clone(&names);
        ANNOTATION_MEMBER_ORDER_MEMO.with(vm, |table| {
            table.entry(cid.as_u32()).or_default().declared = Some(row_names);
        });
    }
    names
}

/// Drop the memoized member facts of every class in `ids` (unloaded or
/// redefined).
pub fn forget_annotation_member_orders(vm: usize, ids: &rustc_hash::FxHashSet<u32>) {
    if ids.is_empty() || !ANNOTATION_MEMBER_ORDER_MEMO.has_row(vm) {
        return;
    }
    ANNOTATION_MEMBER_ORDER_MEMO.with(vm, |table| {
        table.retain(|cid, _| !ids.contains(cid));
    });
}

/// The annotation interface whose `memberDefaults` order the carrier's
/// `toString` must follow, and how it was found. The carrier's type mirror
/// (`ANN_PROXY_TYPE_MIRROR`) is the answer when the class it names declares
/// every member the carrier holds; otherwise (no mirror, a mirror that does
/// not resolve, or one naming a class without those members) the class named
/// by the carrier's type descriptor (`ANN_PROXY_TYPE_DESC`) is tried under the
/// same check. Round 16 wave 14 (lane annorder): the `--compatible`
/// `R16ProxyIdentity` line kept a non-HotSpot order where every other probe got
/// the `memberDefaults` order; this makes the type lookup independent of the
/// one mirror slot. `CRATONVM_ANNOTATION_CDS_MEMBER_ORDER=0` keeps the mirror
/// only.
pub fn annotation_type_declaring_members(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
    mirror_cid: Option<ClassId>,
    elems: &[(String, Value)],
) -> (Option<ClassId>, &'static str) {
    fn declares_all(ctx: &mut dyn NativeContext, cid: ClassId, elems: &[(String, Value)]) -> bool {
        let names = annotation_declared_method_names(ctx, cid);
        elems.iter().all(|(n, _)| names.iter().any(|m| **m == **n))
    }
    if let Some(cid) = mirror_cid {
        if declares_all(ctx, cid, elems) {
            return (Some(cid), "mirror");
        }
    }
    let desc = match ctx.get_field(carrier, ANN_PROXY_TYPE_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let by_desc = desc
        .strip_prefix('L')
        .and_then(|d| d.strip_suffix(';'))
        .and_then(|internal| ctx.class_id_by_name(internal));
    if let Some(cid) = by_desc {
        if Some(cid) != mirror_cid && declares_all(ctx, cid, elems) {
            return (Some(cid), "descriptor");
        }
    }
    (mirror_cid, "mirror-unchecked")
}

/// `CRATONVM_DBG_ANNORDER` (default off): `[annorder]` stderr lines for each
/// annotation-carrier `toString` (`AnnotationInvocationHandler` semantics): the annotation
/// type and how it was found, its defaulted members, whether two share a
/// `memberDefaults` bucket, the CDS index size and each defaulted name's
/// archive offset, and the final member order. Round 16 wave 14 (lane
/// annorder).
pub fn annotation_order_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = false;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}


/// `AnnotationInvocationHandler.quote(char)` for a `char` member: `'` escaped,
/// `"` not; printable ASCII as is, everything else `\uXXXX`.
pub fn jdk_quote_char(c: u16, in_string: bool) -> String {
    match c {
        0x08 => "\\b".to_string(),
        0x0c => "\\f".to_string(),
        0x0a => "\\n".to_string(),
        0x0d => "\\r".to_string(),
        0x09 => "\\t".to_string(),
        0x5c => "\\\\".to_string(),
        0x27 if !in_string => "\\'".to_string(),
        0x22 if in_string => "\\\"".to_string(),
        0x20..=0x7e => char::from(c as u8).to_string(),
        _ => format!("\\u{:04x}", c),
    }
}

pub fn jdk_char_source(c: u16) -> String {
    format!("'{}'", jdk_quote_char(c, false))
}

pub fn jdk_string_source(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for unit in s.encode_utf16() {
        out.push_str(&jdk_quote_char(unit, true));
    }
    out.push('"');
    out
}

pub fn jdk_float_source(f: f32) -> String {
    if f.is_finite() {
        format!("{}f", cratonvm_types::java_float_to_string(f))
    } else if f.is_nan() {
        "0.0f/0.0f".to_string()
    } else if f < 0.0 {
        "-1.0f/0.0f".to_string()
    } else {
        "1.0f/0.0f".to_string()
    }
}

pub fn jdk_double_source(d: f64) -> String {
    if d.is_finite() {
        cratonvm_types::java_double_to_string(d)
    } else if d.is_nan() {
        "0.0/0.0".to_string()
    } else if d < 0.0 {
        "-1.0/0.0".to_string()
    } else {
        "1.0/0.0".to_string()
    }
}

/// `(byte)0x%02x` -- `Formatter` prints a negative `Byte` as its unsigned value.
pub fn jdk_byte_source(b: i32) -> String {
    format!("(byte)0x{:02x}", b as i8 as u8)
}

/// `AnnotationInvocationHandler.memberValueToString` for one member value.
pub fn jdk_member_value_to_string(
    ctx: &mut dyn NativeContext,
    val: Value,
) -> Result<String, MethodCallFailed> {
    use cratonvm_types::ArrayElementType as AET;
    let obj = match val {
        Value::Object(Some(obj)) => obj,
        Value::Int(i) => return Ok(i.to_string()),
        Value::Long(l) => return Ok(format!("{l}L")),
        Value::Float(f) => return Ok(jdk_float_source(f)),
        Value::Double(d) => return Ok(jdk_double_source(d)),
        _ => return Ok("null".to_string()),
    };
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        let et = ctx.heap_element_type_of(obj);
        let n = ctx.array_length(obj);
        let mut parts: Vec<String> = Vec::with_capacity(n);
        for i in 0..n {
            let elem = ctx.get_array_element(obj, i);
            let text = match (et, elem) {
                (AET::Boolean, Value::Int(v)) => (v != 0).to_string(),
                (AET::Byte, Value::Int(v)) => jdk_byte_source(v),
                (AET::Char, Value::Int(v)) => jdk_char_source(v as u16),
                (AET::Short | AET::Int, Value::Int(v)) => v.to_string(),
                (AET::Long, Value::Long(v)) => format!("{v}L"),
                (AET::Float, Value::Float(v)) => jdk_float_source(v),
                (AET::Double, Value::Double(v)) => jdk_double_source(v),
                (_, other) => jdk_member_value_to_string(ctx, other)?,
            };
            parts.push(text);
        }
        return Ok(format!("{{{}}}", parts.join(", ")));
    }
    if let Some(carrier) = annotation_carrier_of(ctx, obj) {
        return ctx_annotation_proxy_to_string(ctx, carrier);
    }
    let cname = ctx_class_name_of(ctx, obj);
    match cname.as_str() {
        "java/lang/String" => {
            if let Some(s) = ctx.read_string(obj) {
                return Ok(jdk_string_source(&s));
            }
        }
        // `Class`: the twin's canonical-name rendering, array-aware.
        "java/lang/Class" => return ctx_format_annotation_value(ctx, val),
        "java/lang/Boolean" => {
            return Ok(matches!(ctx.get_field(obj, 0), Value::Int(v) if v != 0).to_string());
        }
        "java/lang/Character" => {
            if let Value::Int(v) = ctx.get_field(obj, 0) {
                return Ok(jdk_char_source(v as u16));
            }
        }
        "java/lang/Byte" => {
            if let Value::Int(v) = ctx.get_field(obj, 0) {
                return Ok(jdk_byte_source(v));
            }
        }
        "java/lang/Short" | "java/lang/Integer" => {
            if let Value::Int(v) = ctx.get_field(obj, 0) {
                return Ok(v.to_string());
            }
        }
        "java/lang/Long" => {
            if let Value::Long(v) = ctx.get_field(obj, 0) {
                return Ok(format!("{v}L"));
            }
        }
        "java/lang/Float" => {
            if let Value::Float(v) = ctx.get_field(obj, 0) {
                return Ok(jdk_float_source(v));
            }
        }
        "java/lang/Double" => {
            if let Value::Double(v) = ctx.get_field(obj, 0) {
                return Ok(jdk_double_source(v));
            }
        }
        _ => {}
    }
    // An enum constant (its `name`), or whatever the twin makes of the rest.
    ctx_format_annotation_value(ctx, val)
}

/// `Float.floatToIntBits` / `Double.doubleToLongBits`: one canonical NaN.
pub fn jdk_float_bits(f: f32) -> i32 {
    if f.is_nan() {
        0x7fc0_0000
    } else {
        f.to_bits() as i32
    }
}

pub fn jdk_double_hash(d: f64) -> i32 {
    let bits = if d.is_nan() {
        0x7ff8_0000_0000_0000u64
    } else {
        d.to_bits()
    };
    (bits ^ (bits >> 32)) as i32
}

pub fn jdk_long_hash(l: i64) -> i32 {
    (l ^ ((l as u64) >> 32) as i64) as i32
}

/// `AnnotationInvocationHandler.memberValueHashCode`.
pub fn jdk_member_value_hash(ctx: &mut dyn NativeContext, val: Value) -> Result<i32, MethodCallFailed> {
    use cratonvm_types::ArrayElementType as AET;
    let obj = match val {
        Value::Object(Some(obj)) => obj,
        Value::Int(i) => return Ok(i),
        Value::Long(l) => return Ok(jdk_long_hash(l)),
        Value::Float(f) => return Ok(jdk_float_bits(f)),
        Value::Double(d) => return Ok(jdk_double_hash(d)),
        _ => return Ok(0),
    };
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // `Arrays.hashCode` of the array's own type.
        let et = ctx.heap_element_type_of(obj);
        let n = ctx.array_length(obj);
        let mut h: i32 = 1;
        for i in 0..n {
            let elem = ctx.get_array_element(obj, i);
            let eh = match (et, elem) {
                (AET::Boolean, Value::Int(v)) => {
                    if v != 0 {
                        1231
                    } else {
                        1237
                    }
                }
                (AET::Reference, Value::Object(None)) => 0,
                (_, other) => jdk_member_value_hash(ctx, other)?,
            };
            h = h.wrapping_mul(31).wrapping_add(eh);
        }
        return Ok(h);
    }
    if let Some(carrier) = annotation_carrier_of(ctx, obj) {
        return jdk_annotation_hash(ctx, carrier);
    }
    let cname = ctx_class_name_of(ctx, obj);
    match cname.as_str() {
        "java/lang/String" => {
            if let Some(s) = ctx.read_string(obj) {
                return Ok(ctx_java_string_hash(&s));
            }
        }
        "java/lang/Boolean" => {
            return Ok(
                if matches!(ctx.get_field(obj, 0), Value::Int(v) if v != 0) {
                    1231
                } else {
                    1237
                },
            );
        }
        "java/lang/Character"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Integer"
        | "java/lang/Long"
        | "java/lang/Float"
        | "java/lang/Double" => {
            let raw = ctx.get_field(obj, 0);
            return jdk_member_value_hash(ctx, raw);
        }
        _ => {}
    }
    // `Class` and enum constants: `Object.hashCode`, the identity hash.
    Ok(ctx.identity_hash_code(obj))
}

/// `AnnotationInvocationHandler.hashCodeImpl` over the carrier's members.
pub fn jdk_annotation_hash(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let elems = ctx_annotation_proxy_elements(ctx, carrier)?;
    let mut h: i32 = 0;
    for (name, val) in elems {
        let vh = jdk_member_value_hash(ctx, val)?;
        h = h.wrapping_add(127i32.wrapping_mul(ctx_java_string_hash(&name)) ^ vh);
    }
    Ok(h)
}

/// `Objects.equals` on two member values (or two array elements).
pub fn jdk_member_values_equal(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    let (x, y) = match (a, b) {
        (Value::Object(Some(x)), Value::Object(Some(y))) => (x, y),
        (Value::Object(None), Value::Object(None)) => return Ok(true),
        (Value::Int(p), Value::Int(q)) => return Ok(p == q),
        (Value::Long(p), Value::Long(q)) => return Ok(p == q),
        (Value::Float(p), Value::Float(q)) => return Ok(jdk_float_bits(p) == jdk_float_bits(q)),
        (Value::Double(p), Value::Double(q)) => {
            return Ok((p.is_nan() && q.is_nan()) || p.to_bits() == q.to_bits());        }
        _ => return Ok(false),
    };
    if x == y {
        return Ok(true);
    }
    let xa = ctx.heap_kind_of(x) == cratonvm_types::ObjectKind::Array;
    let ya = ctx.heap_kind_of(y) == cratonvm_types::ObjectKind::Array;
    if xa || ya {
        if !(xa && ya) {
            return Ok(false);
        }
        let (xt, yt) = (ctx.heap_element_type_of(x), ctx.heap_element_type_of(y));
        if xt != yt {
            return Ok(false);
        }
        let n = ctx.array_length(x);
        if n != ctx.array_length(y) {
            return Ok(false);
        }
        for i in 0..n {
            let (p, q) = (ctx.get_array_element(x, i), ctx.get_array_element(y, i));
            if !jdk_member_values_equal(ctx, p, q)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    match (annotation_carrier_of(ctx, x), annotation_carrier_of(ctx, y)) {
        (Some(cx), Some(cy)) => return jdk_annotation_equals(ctx, cx, cy),
        (Some(_), None) | (None, Some(_)) => return Ok(false),
        (None, None) => {}
    }
    let (xn, yn) = (ctx_class_name_of(ctx, x), ctx_class_name_of(ctx, y));
    if ctx_wrapper_class_to_primitive(&xn).is_some() {
        if xn != yn {
            return Ok(false);
        }
        let (p, q) = (ctx.get_field(x, 0), ctx.get_field(y, 0));
        return jdk_member_values_equal(ctx, p, q);
    }
    // String, Class and enum constants: the twin's comparison (content,
    // class identity, constant identity by class and name).
    ctx_annotation_values_equal(ctx, a, b)
}

/// `AnnotationInvocationHandler.equalsImpl` between two carriers: same
/// annotation type, and every member of `a` equal to `b`'s.
pub fn jdk_annotation_equals(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    fn type_of(ctx: &dyn NativeContext, carrier: ObjectRef) -> Option<ClassId> {
        match ctx.get_field(carrier, ANN_PROXY_TYPE_MIRROR) {
            Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
            _ => None,
        }
    }
    fn desc_of(ctx: &dyn NativeContext, carrier: ObjectRef) -> String {
        match ctx.get_field(carrier, ANN_PROXY_TYPE_DESC) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    }
    match (type_of(&*ctx, a), type_of(&*ctx, b)) {
        (Some(ta), Some(tb)) if ta != tb => return Ok(false),
        (Some(_), Some(_)) => {}
        _ => {
            if desc_of(&*ctx, a) != desc_of(&*ctx, b) {
                return Ok(false);
            }
        }
    }
    let a_elems = ctx_annotation_proxy_elements(ctx, a)?;
    let b_elems = ctx_annotation_proxy_elements(ctx, b)?;
    for (name, av) in &a_elems {
        let Some((_, bv)) = b_elems.iter().find(|(n, _)| n == name) else {
            return Ok(false);
        };
        if !jdk_member_values_equal(ctx, *av, *bv)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `hashCode()` / `equals(Object)` on the annotation carrier
/// `carrier` with `AnnotationInvocationHandler` semantics, for
/// `vm_exec.rs`'s `annotation_proxy_invoke_shared` (interpreter conventions:
/// a raw `int`, a raw `boolean`). `None` when the switch is off,
/// for any other method, and for `equals` whose argument is a FOREIGN object
/// (not a VM annotation) while `CRATONVM_ANNOTATION_FOREIGN_EQUALS=0`: the
/// caller then keeps its delegation to the other side.
pub fn annotation_carrier_object_method(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if !annotation_jdk_semantics() {
        return None;
    }
    match (method_name, args) {
        ("hashCode", []) => Some(jdk_annotation_hash(ctx, carrier).map(|h| Some(Value::Int(h)))),
        ("equals", [other]) => {
            let other_carrier = match *other {
                Value::Object(Some(o)) => match annotation_carrier_of(ctx, o) {
                    Some(c) => Some(c),
                    None if annotation_foreign_equals_reflective() => {
                        let eq = jdk_annotation_equals_foreign(ctx, carrier, o, 0);
                        return Some(eq.map(|eq| Some(Value::Int(i32::from(eq)))));                    }
                    None => return None,                },
                _ => None,
            };
            let eq = match other_carrier {
                Some(c) => jdk_annotation_equals(ctx, carrier, c),
                None => Ok(false),
            };
            Some(eq.map(|eq| Some(Value::Int(i32::from(eq)))))
        }
        _ => None,
    }
}

/// `toString()` of the annotation carrier with `AnnotationInvocationHandler`
/// semantics, as text: `vm_exec.rs` makes the (un-interned) string itself.
pub fn annotation_carrier_to_string(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
) -> Result<String, MethodCallFailed> {
    ctx_annotation_proxy_to_string(ctx, carrier)
}

/// An annotation member's value as `AnnotationInvocationHandler.invoke`
/// returns it: a non-empty array is a fresh clone, so a caller's write never
/// reaches the annotation (switch `CRATONVM_ANNOTATION_JDK_SEMANTICS`).
pub fn annotation_member_value_copy(ctx: &mut dyn NativeContext, value: Value) -> MethodCallResult {
    if annotation_jdk_semantics() {
        if let Value::Object(Some(arr)) = value {
            let is_array = ctx.heap_kind_of(arr) == cratonvm_types::ObjectKind::Array;
            if is_array && ctx.array_length(arr) > 0 {
                return crate::native_object_clone(ctx, &[value]);
            }
        }
    }
    Ok(Some(value))
}

// ---------------------------------------------------------------------------
// Round 13 wave 8 (lane proxy5): the rest of `AnnotationInvocationHandler`'s
// contract for the VM's annotation carrier.
//
// * `equalsImpl` against an object that is not one of this VM's annotations
//   (a user class implementing the annotation interface, or a proxy whose
//   handler is not ours, e.g. Spring's synthesized annotations) reads every
//   member off the other object (`memberMethod.invoke(o)`) and compares it
//   with `memberValueEquals`. Both doors used to answer `other.equals(this)`
//   instead: `false` for any implementation that keeps `Object.equals`, and
//   the VM's bare carrier leaked to foreign code as the argument.
// * A member of the annotation interface with neither a value nor a default
//   (the annotated class was compiled against an older interface) throws
//   `IncompleteAnnotationException(type, member)` on access; it read `null`.
// ---------------------------------------------------------------------------

/// `CRATONVM_ANNOTATION_FOREIGN_EQUALS` (default on, both modes): annotation
/// `equals` against a foreign implementation compares members reflectively.
/// `0` restores the delegation to `other.equals(this)`.
pub fn annotation_foreign_equals_reflective() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `CRATONVM_ANNOTATION_CARRIER_IS_HANDLER` (default on, both modes; also read
/// once by `ClassManager::fabricate_class`, which makes the carrier implement
/// `InvocationHandler`): with real annotations the carrier is only ever an
/// annotation proxy's handler, so its `getClass()` is its own class, as the
/// object header (and so compiled code) already answers. Read by
/// `vm::vm::vm_exec`'s `annotation_proxy_dispatch_impl`. Off, or with
/// `CRATONVM_REAL_ANNOTATIONS=0`, the carrier answers the annotation type.
pub fn annotation_carrier_get_class_is_own() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let on = match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    };
    on && real_annotations_enabled()
}

// ---------------------------------------------------------------------------
// Round 16 wave 16 (lane proxy6): `Proxy.getInvocationHandler(annotation)`
// answers a real `sun.reflect.annotation.AnnotationInvocationHandler`.
//
// The VM's annotation `$ProxyN` keeps its carrier (`AnnotationProxy`) in the
// handler slot: every intercept in the interpreter, the JIT and the generated
// body's `Proxy$Dispatch.invokeProxy` keys on it, and Spring's attribute
// adaptation was built around it. What Java code can OBSERVE of the handler is
// only what `Proxy.getInvocationHandler` hands out, so that door now hands out
// HotSpot's object instead: an `AnnotationInvocationHandler` built through its
// real constructor from the annotation type and a `LinkedHashMap` of the
// carrier's member values in `memberValues` order
// ([`jdk_annotation_members_in_map_order`], waves 13-14). Its class, its
// `toString` / `hashCode` / `equals` (`Object`'s) and its `invoke` (the JDK's
// own `equalsImpl` / `toStringImpl` / `hashCodeImpl` / member read with the
// array clone) are therefore HotSpot's by construction, and agree with the
// carrier because they read the carrier's own values.
//
// One handler per proxy, built on first request and kept in a per-VM row keyed
// by the proxy's address (`getInvocationHandler(a) == getInvocationHandler(a)`
// as on HotSpot). The row is rooted under the annotation type's class (a live
// proxy holds that class's mirror in its interfaces array, so the handler
// lives exactly as long as it can be asked for), remapped and pruned with the
// annotation-proxy cache, and dropped at VM teardown and class unloading.
//
// Not built (the carrier is handed out, as before) when a member value is a
// deferred `TypeNotPresentException` (the JDK would hold an `ExceptionProxy`
// built from the type name this VM did not keep), when the type is
// unreachable, or when the JDK class or its constructor is unavailable (a
// synthetic JDK). `CRATONVM_ANNOTATION_JDK_HANDLER=0` restores the carrier.
// ---------------------------------------------------------------------------

/// `CRATONVM_ANNOTATION_JDK_HANDLER` (default on, both modes): see the section
/// comment above. `0` hands out the carrier.
pub fn annotation_jdk_handler() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The JDK class handed out as an annotation proxy's handler.
pub const JDK_ANNOTATION_HANDLER_CLASS: &str = "sun/reflect/annotation/AnnotationInvocationHandler";

/// One handed-out handler: the `AnnotationInvocationHandler` and the class id
/// of its annotation type (the GC owner of the row).
#[derive(Clone, Copy)]
pub struct JdkHandlerRow {
    pub handler: ObjectRef,
    pub owner: u32,
}

// Per VM: generated annotation proxy address -> its handed-out handler.
// addr-keyed: REMAPPED by gc_update_annotation_jdk_handlers, PRUNED by prune_dead_annotation_jdk_handlers / forget_unloaded_annotation_jdk_handlers.
pub static ANNOTATION_JDK_HANDLERS: VmScoped<FxHashMap<usize, JdkHandlerRow>> = VmScoped::new();

/// The value `Proxy.getInvocationHandler(proxy)` hands to Java for a generated
/// proxy `proxy`: the real `AnnotationInvocationHandler` when `proxy` is one of
/// this VM's annotation proxies (see the section comment), else the proxy's
/// handler slot as stored. Read the slot through this, not before it: building
/// the handler allocates and runs Java, so `proxy` may move.
pub fn proxy_invocation_handler_for_java(ctx: &mut dyn NativeContext, proxy: ObjectRef) -> Value {
    if !annotation_jdk_handler() {
        return ctx.get_field(proxy, 0);
    }
    let carrier = match annotation_carrier_of(ctx, proxy) {
        Some(carrier) if carrier != proxy => carrier,
        _ => return ctx.get_field(proxy, 0),
    };
    if let Some(handler) = cached_jdk_handler(ctx, proxy, carrier) {
        return Value::Object(Some(handler));
    }
    let mut scope = NativeHandleScope::new(ctx);
    let proxy_h = scope.root(proxy);
    let built = build_jdk_annotation_handler(&mut scope, carrier);
    let proxy = scope.get(&proxy_h);
    match built {
        Some((handler, owner)) => {
            let vm = scope.vm_identity();
            let key = proxy.as_ptr() as usize;
            let winner = ANNOTATION_JDK_HANDLERS.with(vm, |table| {
                // A racing thread's row wins, so every caller sees one handler.
                table
                    .entry(key)
                    .or_insert(JdkHandlerRow {
                        handler,
                        owner: owner.as_u32(),
                    })
                    .handler
            });
            Value::Object(Some(winner))
        }
        None => scope.get_field(proxy, 0),
    }
}

/// The row of `proxy`, when it still describes `carrier`'s annotation: its
/// handler's `type` is the carrier's type mirror. A mismatch (an address
/// reused by another annotation's proxy between a death and the next prune)
/// answers `None`, and the caller rebuilds.
pub fn cached_jdk_handler(
    ctx: &mut dyn NativeContext,
    proxy: ObjectRef,
    carrier: ObjectRef,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let key = proxy.as_ptr() as usize;
    let row = ANNOTATION_JDK_HANDLERS
        .peek(vm, |table| table.get(&key).copied())
        .flatten()?;
    let carrier_type = match ctx.get_field(carrier, ANN_PROXY_TYPE_MIRROR) {
        Value::Object(Some(mirror)) => mirror,
        _ => return None,
    };
    match ctx.get_field_by_name(row.handler, "type") {
        Value::Object(Some(t)) if t == carrier_type => Some(row.handler),
        _ => {
            ANNOTATION_JDK_HANDLERS.with(vm, |table| {
                table.remove(&key);
            });
            None
        }
    }
}

/// `new AnnotationInvocationHandler(type, memberValues)` for `carrier`, with
/// the annotation type's class id. `None` when it cannot or must not be built
/// (see the section comment); nothing is thrown.
pub fn build_jdk_annotation_handler(
    scope: &mut NativeHandleScope<'_>,
    carrier: ObjectRef,
) -> Option<(ObjectRef, ClassId)> {
    let carrier_h = scope.root(carrier);
    let type_mirror = match scope.get_field(carrier, ANN_PROXY_TYPE_MIRROR) {
        Value::Object(Some(mirror)) => mirror,
        _ => return None,
    };
    let type_cid = scope.class_id_from_mirror(type_mirror)?;
    let type_h = scope.root(type_mirror);
    // Names in `memberValues` order. The values are re-read below, after the
    // order computation, so every one is current when it is rooted.
    let carrier = scope.get(&carrier_h);
    let order: Vec<String> = jdk_annotation_members_in_map_order(&mut **scope, carrier)
        .ok()?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let carrier = scope.get(&carrier_h);
    let elems = ctx_annotation_proxy_elements(&mut **scope, carrier).ok()?;
    let throwable = scope.class_id_by_name("java/lang/Throwable");
    let mut members: Vec<(String, cratonvm_native_api::NativeHandle)> =
        Vec::with_capacity(order.len());
    for name in order {
        // A member with no value (neither explicit nor defaulted) has no
        // `memberValues` entry: `invoke` throws `IncompleteAnnotationException`.
        let Some(Value::Object(Some(value))) =
            elems.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
        else {
            continue;
        };
        if holds_deferred_type_not_present(&**scope, value, throwable) {
            return None;
        }
        let handle = scope.root(value);
        members.push((name, handle));
    }
    let map = match scope.new_object_initialized("java/util/LinkedHashMap", "()V", &[]) {
        Ok(Some(Value::Object(Some(map)))) => map,
        _ => return None,
    };
    let map_h = scope.root(map);
    for (name, value_h) in &members {
        // Interned: HotSpot's `memberValues` keys are the interned method names,
        // and `AnnotationInvocationHandler.invoke` compares them by identity.
        let key = scope.intern_string(name);
        let args = [
            Value::Object(Some(key)),
            Value::Object(Some(scope.get(value_h))),
        ];
        let map = scope.get(&map_h);
        scope
            .invoke_virtual(
                map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &args,
            )
            .ok()?;
    }
    let args = [
        Value::Object(Some(scope.get(&type_h))),
        Value::Object(Some(scope.get(&map_h))),
    ];
    match scope.new_object_initialized(
        JDK_ANNOTATION_HANDLER_CLASS,
        "(Ljava/lang/Class;Ljava/util/Map;)V",
        &args,
    ) {
        Ok(Some(Value::Object(Some(handler)))) => Some((handler, type_cid)),
        _ => None,
    }
}

/// Whether a carrier member value is (or, for a reference array, holds) one of
/// the deferred `TypeNotPresentException`s the builder stores for an
/// unresolvable `Class` member.
pub fn holds_deferred_type_not_present(
    ctx: &dyn NativeContext,
    value: ObjectRef,
    throwable: Option<ClassId>,
) -> bool {
    let Some(throwable) = throwable else {
        return false;
    };
    let is_throwable = |ctx: &dyn NativeContext, obj: ObjectRef| {
        ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Object
            && ctx.is_subclass(ctx.class_id_of_object(obj), throwable)
    };
    match ctx.heap_kind_of(value) {
        cratonvm_types::ObjectKind::Array => (0..ctx.array_length(value)).any(|i| {
            matches!(ctx.get_array_element(value, i), Value::Object(Some(e)) if is_throwable(ctx, e))
        }),
        _ => is_throwable(ctx, value),
    }
}

/// GC root scan half of [`ANNOTATION_JDK_HANDLERS`]: each handler under its
/// annotation type's class.
pub fn gc_scan_annotation_jdk_handlers(
    vm_identity: usize,
    visit: &mut dyn FnMut(Option<u32>, ObjectRef),
) {
    ANNOTATION_JDK_HANDLERS.peek(vm_identity, |table| {
        for row in table.values() {
            visit(Some(row.owner), row.handler);
        }
    });
}

/// Post-move remap of [`ANNOTATION_JDK_HANDLERS`]: keys (proxy addresses) and
/// handlers.
pub fn gc_update_annotation_jdk_handlers(vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
    if !ANNOTATION_JDK_HANDLERS.has_row(vm_identity) {
        return;
    }
    ANNOTATION_JDK_HANDLERS.with(vm_identity, |table| {
        let mut remapped = FxHashMap::default();
        for (proxy_addr, mut row) in table.drain() {
            let old = row.handler.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                row.handler = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
            let new_proxy_addr = *pointer_map.get(&proxy_addr).unwrap_or(&proxy_addr);
            remapped.insert(new_proxy_addr, row);
        }
        *table = remapped;
    });
}

/// Drop the [`ANNOTATION_JDK_HANDLERS`] rows whose proxy or handler did not
/// survive (`is_live` over PRE-remap addresses). Returns the rows dropped.
pub fn prune_dead_annotation_jdk_handlers(
    vm_identity: usize,
    is_live: &dyn Fn(usize) -> bool,
) -> usize {
    if !ANNOTATION_JDK_HANDLERS.has_row(vm_identity) {
        return 0;
    }
    ANNOTATION_JDK_HANDLERS.with(vm_identity, |table| {
        let before = table.len();
        table.retain(|&proxy_addr, row| {
            is_live(proxy_addr) && is_live(row.handler.as_ptr() as usize)
        });
        before - table.len()
    })
}

/// Drop the [`ANNOTATION_JDK_HANDLERS`] rows of unloaded annotation types and
/// of the proxies the annotation cache just dropped.
pub fn forget_unloaded_annotation_jdk_handlers(
    vm_identity: usize,
    dead: &rustc_hash::FxHashSet<u32>,
    dropped_proxies: &[ObjectRef],
) {
    if !ANNOTATION_JDK_HANDLERS.has_row(vm_identity) {
        return;
    }
    // addr-keyed: local -- the proxies the cache dropped in this same call.
    let dropped: rustc_hash::FxHashSet<usize> = dropped_proxies
        .iter()
        .map(|p| p.as_ptr() as usize)
        .collect();
    ANNOTATION_JDK_HANDLERS.with(vm_identity, |table| {
        table.retain(|proxy_addr, row| !dead.contains(&row.owner) && !dropped.contains(proxy_addr));
    });
}

/// `CRATONVM_ANNOTATION_MEMBER_CONFORMANCE` (default on, both modes): a
/// member that has neither a value nor a default throws
/// `IncompleteAnnotationException`, and a member named `getType()` is read as
/// a member (`vm_exec.rs`' carrier arm answers the annotation type only for an
/// interface that declares no such member). `0` restores `null` and the arm.
pub fn annotation_member_conformance() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `AnnotationInvocationHandler.equalsImpl(proxy, other)` for the carrier
/// `carrier`, `other` being any object: a VM annotation compares carrier to
/// carrier, anything else member by member through its own accessors. For the
/// generated-body door (`reflect_annotations::native_proxy_dispatch_invoke`).
pub fn annotation_equals_object(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
    other: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if other == carrier {
        return Ok(true);
    }
    match annotation_carrier_of(ctx, other) {
        Some(c) => jdk_annotation_equals(ctx, carrier, c),
        None => jdk_annotation_equals_foreign(ctx, carrier, other, 0),
    }
}

/// `equalsImpl` when `other` is not a VM annotation: `type.isInstance(other)`,
/// then every member of the annotation interface is read off `other` by a
/// virtual call and compared with the carrier's value. An accessor that throws
/// answers `false` (the JDK catches `InvocationTargetException`). A nested
/// member that is a VM annotation against a foreign one recurses; an ARRAY of
/// annotations with foreign elements compares unequal (the JDK would ask each
/// element's `equals`).
pub fn jdk_annotation_equals_foreign(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
    other: ObjectRef,
    depth: u32,
) -> Result<bool, MethodCallFailed> {
    const ACC_STATIC: u16 = 0x0008;
    const ACC_SYNTHETIC: u16 = 0x1000;
    if depth > 16 || ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object {
        return Ok(false);
    }
    let type_cid = match ctx.get_field(carrier, ANN_PROXY_TYPE_MIRROR) {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    let Some(type_cid) = type_cid else {
        return Ok(false);
    };
    let other_cid = ctx.class_id_of_object(other);
    if other_cid != type_cid && !ctx.is_subclass(other_cid, type_cid) {
        return Ok(false);
    }
    let members: Vec<(String, String)> = ctx
        .declared_methods(type_cid)
        .into_iter()
        .filter(|m| {
            !m.name.starts_with('<')
                && m.descriptor.starts_with("()")
                && m.access_flags & (ACC_STATIC | ACC_SYNTHETIC) == 0
        })
        .map(|m| (m.name, m.descriptor))
        .collect();
    // Each accessor call runs Java; the carrier and `other` live in handles.
    let mut scope = NativeHandleScope::new(ctx);
    let carrier_h = scope.root(carrier);
    let other_h = scope.root(other);
    for (name, desc) in &members {
        let receiver = scope.get(&other_h);
        let his = match scope.invoke_virtual(receiver, name, desc, &[]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(MethodCallFailed::ExceptionThrown(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        // Nothing from here to the comparison allocates, so `his` stays current.
        let carrier_now = scope.get(&carrier_h);
        let elems = ctx_annotation_proxy_elements(&mut *scope, carrier_now)?;
        let Some(ours) = elems.into_iter().find(|(n, _)| n == name).map(|(_, v)| v) else {
            return Ok(false);
        };
        // A primitive member comes back raw from the accessor; the carrier
        // stores its wrapper.
        let ours = match (ours, his) {
            (
                Value::Object(Some(o)),
                Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_),
            ) if ctx_wrapper_class_to_primitive(&ctx_class_name_of(&mut *scope, o)).is_some() => {
                scope.get_field(o, 0)
            }
            (v, _) => v,
        };
        let equal = match (ours, his) {
            (Value::Object(Some(o)), Value::Object(Some(h)))
                if ctx_class_name_of(&mut *scope, o) != ctx_class_name_of(&mut *scope, h)
                    && annotation_carrier_of(&mut *scope, h).is_none() =>
            {
                match annotation_carrier_of(&mut *scope, o) {
                    Some(nested) => {
                        jdk_annotation_equals_foreign(&mut *scope, nested, h, depth + 1)?
                    }
                    None => jdk_member_values_equal(&mut *scope, ours, his)?,
                }
            }
            _ => jdk_member_values_equal(&mut *scope, ours, his)?,
        };
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `AnnotationInvocationHandler.invoke`'s `IncompleteAnnotationException`: the
/// exception to throw when `member` is a member of the carrier's annotation
/// interface with no value in the carrier and no `AnnotationDefault`, else
/// `None` (not a member, a default exists, the switch is off, or the exception
/// could not be built -- the caller's `null` stands). For `vm_exec.rs`'
/// `annotation_proxy_invoke_shared`, asked only when a member read came back
/// `null`, so the ordinary read pays nothing.
pub fn annotation_incomplete_member(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
    member: &str,
) -> Option<MethodCallFailed> {
    if !annotation_member_conformance() {
        return None;
    }
    let mirror = match ctx.get_field(carrier, ANN_PROXY_TYPE_MIRROR) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let type_cid = ctx.class_id_from_mirror(mirror)?;
    let declared = ctx.declared_methods(type_cid).into_iter().find(|m| {
        m.name == member && m.descriptor.starts_with("()") && m.access_flags & 0x0008 == 0
    })?;
    if ctx
        .method_annotation_default(type_cid, &declared.name, &declared.descriptor)
        .is_some()
    {
        return None;
    }
    let present = ctx_annotation_proxy_elements(ctx, carrier)
        .ok()?
        .iter()
        .any(|(n, _)| n == member);
    if present {
        return None;
    }
    let class = "java/lang/annotation/IncompleteAnnotationException";
    let mut scope = NativeHandleScope::new(ctx);
    let mirror_h = scope.root(mirror);
    let cid = scope.ensure_class_initialized(class).ok()?;
    let n_fields = scope.class_num_total_fields(cid).max(4);
    let exc = scope.alloc_object(cid, n_fields);
    let exc_h = scope.root(exc);
    let name = scope.create_string(member);
    let ctor_args = [
        Value::Object(Some(scope.get(&exc_h))),
        Value::Object(Some(scope.get(&mirror_h))),
        Value::Object(Some(name)),
    ];
    scope
        .invoke(
            class,
            "<init>",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
            &ctor_args,
        )
        .ok()?;
    Some(MethodCallFailed::ExceptionThrown(scope.get(&exc_h)))
}

/// Build a `java.lang.TypeNotPresentException(typeName, cause)` to store as a
/// Class-valued annotation member that could NOT be resolved through the
/// declaring class's loader (classloader-isolation / filtering). It is stored as
/// the member value, NOT thrown here вЂ” `annotation_proxy_dispatch_impl` detects
/// it and throws it when the member is accessed, mirroring HotSpot's deferred
/// `TypeNotPresentExceptionProxy` (so `getAnnotations()` does not throw, only the
/// member accessor does). `type_name` is the binary (dotted) name; `cause` is the
/// `ClassNotFoundException` the loader raised.
pub fn make_type_not_present_exception(
    ctx: &mut dyn NativeContext,
    type_name: &str,
    cause: Option<ObjectRef>,
    capture_trace: bool,
) -> Option<ObjectRef> {
    let cid = ctx
        .class_id_by_name("java/lang/TypeNotPresentException")
        .or_else(|| {
            let _ = ctx.load_class("java/lang/TypeNotPresentException");
            ctx.class_id_by_name("java/lang/TypeNotPresentException")
        })?;
    let nfields = ctx.class_num_total_fields(cid).max(4);
    let exc = ctx.alloc_object(cid, nfields);
    // `exc` (and `cause`, when present) are raw `ObjectRef`s held across
    // several re-entrant, potentially-allocating calls below (`create_string`,
    // the `write_throwable_*` field writers). Under the moving collector any
    // of those can trigger a GC that relocates `exc`/`cause`, leaving the
    // Rust-local copy stale — it then reads back as whatever unrelated object
    // now occupies that memory (observed as `gen_heap::get_field` "undersized
    // object layout" errors on unrelated `String` objects, since this path is
    // exercised far more often — once per unresolvable `@ConditionalOnClass`
    // element — than the narrow classloader-isolation call site that
    // originally used this helper). Pin both across the whole construction
    // and re-fetch after every call that can allocate, per the documented
    // `pin_native_root`/`read_native_pin` contract.
    let pin = ctx.pin_native_root(exc);
    let cause_pin = cause.map(|c| ctx.pin_native_root(c));
    // Mirror `new TypeNotPresentException(type, cause)`: message + cause set via
    // the layout-aware Throwable helpers (Throwable.<init> is shadowed, so the
    // real field initializers do not run otherwise).
    let msg = format!("Type {type_name} not present");
    let msg_obj = ctx.create_string(&msg);
    let exc = ctx.read_native_pin(pin, exc);
    let msg_pin = ctx.pin_native_root(msg_obj);
    let msg_obj = ctx.read_native_pin(msg_pin, msg_obj);
    crate::lang_misc::write_throwable_detail_message(ctx, exc, Value::Object(Some(msg_obj)));
    let exc = ctx.read_native_pin(pin, exc);
    if let Some(c) = cause {
        let c = cause_pin.map(|p| ctx.read_native_pin(p, c)).unwrap_or(c);
        crate::lang_misc::write_throwable_cause(ctx, exc, Value::Object(Some(c)));
    }
    let mut exc = ctx.read_native_pin(pin, exc);
    // `capture_throwable_trace` also walks/allocates; keep `exc` pinned
    // through it too. Callers on a hot path (every unresolvable
    // `@ConditionalOnClass`-style element, VM-wide) can skip it — Spring only
    // ever checks `instanceof TypeNotPresentException` / reads the message on
    // this sentinel, never its stack trace — to avoid the extra work, not for
    // correctness (the actual corruption bug was the missing pins above).
    if capture_trace {
        crate::lang_misc::capture_throwable_trace(ctx, &mut exc);
    }
    let exc = ctx.read_native_pin(pin, exc);
    ctx.unpin_native_roots(pin);
    Some(exc)
}

/// Each VM's sentinel global-root handle.
///
/// gc-common w20-f: this was ONE process-wide `Mutex<Option<usize>>`, and it
/// was wrong three ways. The handle indexes the publishing VM's own global-ref
/// table, so a second VM resolved VM A's index in ITS table (an unrelated
/// object handed out as the `TypeNotPresentException`); the row outlived VM
/// A; and the builder held the lock across `make_type_not_present_exception`
/// (allocation and Java) and `resolve_global_root`, so a second thread
/// blocked on it as a counted mutator while the first waited for a
/// stop-the-world that could not start without it. Per VM now, and the lock
/// is never held across a `ctx` call. Dropped by
/// [`forget_vm_annotation_proxies`].
pub static UNRESOLVABLE_CLASS_SENTINEL: VmScoped<Option<usize>> = VmScoped::new();

/// Default (no `container_loader`) unresolvable-`Class`-element sentinel —
/// the overwhelmingly common case (`@ConditionalOnClass`/`@ConditionalOnMissingClass`
/// referencing an optional dependency absent from the classpath, with no
/// classloader-isolation involved). Unlike [`make_type_not_present_exception`]
/// (used only by the narrow, rarely-exercised classloader-isolation call
/// site), this path fires once per unresolvable `Class`-typed element on
/// EVERY annotated class VM-wide — allocating a fresh exception object each
/// time turned a per-element loop that previously never allocated at all
/// (an unresolvable class silently became `null`) into one that allocates
/// routinely, which exposed a pre-existing GC-safety gap elsewhere in
/// `create_annotation_proxy`'s array-construction loop (the array/element
/// arrays it fills are held across the loop without being pinned; see
/// `create_annotation_proxy`). Properly pinning the sentinel's OWN
/// construction (as `make_type_not_present_exception` now does) did not
/// fix that — the corruption persisted because the *other*, pre-existing
/// unpinned arrays are what actually go stale once allocation starts
/// happening inside that loop.
///
/// Sidestep the whole class of hazard instead of chasing it: build ONE
/// generic `TypeNotPresentException` instance, ever, and permanently
/// GC-root it (`add_global_root`/`resolve_global_root` — the same
/// mechanism JNI global refs use), so this path allocates at most once per
/// process instead of once per occurrence. After the first call it's a
/// pure lookup — no allocation, so nothing downstream can be exposed to a
/// GC that wasn't already possible before this fix existed. The message is
/// intentionally generic (not per-class) since callers only ever check
/// `instanceof TypeNotPresentException`; nothing here reads the message.
pub fn shared_unresolvable_class_sentinel(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    // Copy the handle out; the table lock is released before any `ctx` call.
    let published = UNRESOLVABLE_CLASS_SENTINEL.peek(vm, |h| *h).flatten();
    if let Some(obj) = published.and_then(|h| ctx.resolve_global_root(h)) {
        return Some(obj);
    }
    let exc = make_type_not_present_exception(ctx, "<annotation-referenced type>", None, true)?;
    let handle = ctx.add_global_root(exc);
    // Publish unless another thread of this VM won the race meanwhile.
    let winner = UNRESOLVABLE_CLASS_SENTINEL.with(vm, |row| match *row {
        Some(existing) if Some(existing) != published => Some(existing),
        _ => {
            *row = Some(handle);
            None
        }
    });
    match winner {
        None => Some(exc),
        Some(existing) => match ctx.resolve_global_root(existing) {
            Some(obj) => {
                ctx.remove_global_root(handle);
                Some(obj)
            }
            // The winner's root is gone; keep ours published instead.
            None => {
                UNRESOLVABLE_CLASS_SENTINEL.with(vm, |row| *row = Some(handle));
                Some(exc)
            }
        },
    }
}

/// Resolve a Class-valued annotation member's class name through the declaring
/// class's (`container`) loader, mirroring HotSpot's
/// `AnnotationParser.parseClassValue(sig, container)`. Returns `Ok(mirror)` on
/// success and `Err(Some(cnfe))` when the loader raised a
/// `ClassNotFoundException` (в†’ a deferred `TypeNotPresentException`).
/// `Err(None)` for any other failure (caller falls back to the global resolve).
pub fn resolve_annotation_class_via_loader(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    class_name: &str,
) -> Result<ObjectRef, Option<ObjectRef>> {
    // Loading through a user-defined loader can allocate and re-enter Java.
    // Keep the loader and the freshly-created class-name String visible to a
    // moving collector until the virtual call has consumed them.
    let loader_pin = ctx.pin_native_root(loader);
    let dotted = class_name.replace('/', ".");
    let name_obj = ctx.create_string(&dotted);
    let name_pin = ctx.pin_native_root(name_obj);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let name_obj = ctx.read_native_pin(name_pin, name_obj);
    let result = match crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_obj) {
        Ok(Some(Value::Object(Some(mirror)))) => Ok(mirror),
        Ok(_) => Err(None),
        // The loader threw вЂ” typically ClassNotFoundException (the filter case).
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc)) => Err(Some(exc)),
        // Internal VM error вЂ” don't synthesize; let the caller fall back.
        Err(_) => Err(None),
    };
    ctx.unpin_native_roots(name_pin);
    ctx.unpin_native_roots(loader_pin);
    result
}

/// The deferred sentinel for an annotation-valued member (or array entry)
/// whose own annotation type is unresolvable. Reuses the process-wide,
/// permanently-rooted `TypeNotPresentException` instance so this path never
/// allocates — see [`shared_unresolvable_class_sentinel`] for why that matters
/// inside the element loop.
pub fn unresolvable_nested_annotation_sentinel(
    ctx: &mut dyn NativeContext,
    nested: &cratonvm_native_api::AnnotationData,
) -> Option<ObjectRef> {
    if crate::nbflags().iae_trace_ok {
        eprintln!(
            "ANN-NESTED-UNRESOLVABLE type_descriptor={} — member deferred to TypeNotPresentException",
            nested.type_descriptor
        );
    }
    shared_unresolvable_class_sentinel(ctx)
}

/// Resolve the `ClassId` backing an annotation's declared TYPE — the single
/// source of truth for [`annotation_type_loadable_near`] (which decides whether
/// an annotation is surfaced at all) and [`create_annotation_proxy_with_type`]
/// (which builds it).
///
/// The two MUST agree. When the filter admits an annotation whose type the
/// builder then fails to resolve, the builder publishes an `AnnotationProxy`
/// with a **null type mirror** — and, because the `AnnotationDefault` backfill
/// is gated on this same ClassId, with none of its defaulted members filled in.
/// Every reflective annotation walker then dereferences a null:
/// Spring's `AnnotationsScanner.isIgnorable(annotation.annotationType())`,
/// JUnit's `AnnotationUtils.findRepeatableAnnotations`
/// (`candidateAnnotationType.equals(containerType)`), and Byte Buddy's
/// `ForFieldBinding.bind` (`declaringType(annotation).represents(void.class)`,
/// where `declaringType()` is exactly such a never-backfilled
/// `Class`-valued member defaulting to `void.class`).
///
/// This is the ADMISSION half — deliberately loader-free. It must not call
/// `loadClass` on a user-defined loader: that is a re-entrant Java call, and
/// running one per annotation per `getDeclaredAnnotations()` (on top of the one
/// the builder already makes) measurably changed behaviour under Mockito's
/// inline retransformer. The builder still does the loader-aware lookup for
/// IDENTITY — it just falls back to this answer instead of to nothing.
///
/// Order: the declaring class's namespace, then the loader-blind global index,
/// then a load. The `_near` retry AFTER the load is what the admission filter
/// and the builder used to disagree about — `class_id_by_name` is
/// `find_unique_class_by_name`, which answers `None` for an AMBIGUOUS name, so
/// the moment a second loader defines the same annotation type the global
/// lookup goes blind while the scoped one still resolves.
pub fn resolve_annotation_type_class_id(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    container_class_id: Option<ClassId>,
) -> Option<ClassId> {
    let resolved = container_class_id
        .and_then(|h| ctx.class_id_by_name_near(class_name, h))
        // JVMS 5.4.3.1: the declaring class's OWN loader initiates the type, so ask
        // it before the process-wide name table. `class_id_by_name` answers the
        // SOLE definition of a name, which is another loader's copy whenever the
        // container's loader has not loaded its own yet (Spring Boot's
        // `ModifiedClassPathClassLoader` forks define `@Order` first; the
        // application loader's `LogbackLoggingSystem$Factory` then reported the
        // fork's `Order`, `OrderUtils.getOrder` found none, the logging system
        // factories stayed in `spring.factories` order and `LoggingSystem.get`
        // answered `JavaLoggingSystem`: `LoggingSystemTests`).
        .or_else(|| {
            container_class_id.and_then(|h| {
                ctx.class_id_by_name_via_referencing_class(h, class_name)
                    .ok()
            })
        })
        .or_else(|| ctx.class_id_by_name(class_name))
        .or_else(|| {
            let _ = ctx.load_class(class_name);
            ctx.class_id_by_name(class_name).or_else(|| {
                container_class_id.and_then(|h| ctx.class_id_by_name_near(class_name, h))
            })
        })
        .or_else(|| {
            // Last resort, mirroring the `Enum`-valued member arm: resolve
            // through the declaring member's INITIATING loader. A bare global
            // lookup can miss an application dependency that is visible to
            // that member and can lose identity under an isolated loader.
            container_class_id.and_then(|h| {
                ctx.class_id_by_name_via_referencing_class(h, class_name)
                    .ok()
            })
        })?;
    // "Resolved" has to mean "resolved to an ANNOTATION TYPE".
    //
    // Every arm above ends in a name lookup, and `ctx.load_class` FABRICATES a
    // synthetic stand-in for a name that is on no classpath — so the filter
    // this function exists to implement could essentially never say
    // "unresolvable", and the compile-only annotation it was written to drop
    // came back as a plain synthetic class instead. A `Proxy` built over that
    // implements a non-interface with no superinterfaces, so the proxy is not a
    // `java.lang.annotation.Annotation` and every consumer that casts to one
    // gets a `ClassCastException` — measured on
    // `org.infinispan.query.remote.client.impl.QueryRequest`, whose
    // `@org.jboss.marshalling.Externalize` is not on the Spring Boot cache
    // classpath: `jdk.proxy1.$Proxy0` with `isInterface=false
    // isAnnotation=false superinterfaces=[]`, against the eight real
    // annotations on the same class which are all `isAnnotation=true
    // superinterfaces=[java.lang.annotation.Annotation]`.
    //
    // The visible damage was Byte Buddy's `AnnotationList$ForLoadedAnnotations`
    // — it casts each element of `getDeclaredAnnotations()` to `Annotation` —
    // which took out Mockito's inline mock maker: `Could not modify all
    // classes`, and for a FINAL class (nothing to subclass) `mock()` then
    // failed outright instead of silently degrading to a subclass proxy.
    //
    // JLS 9.6 gives the test: an annotation interface is an interface that
    // implicitly extends `java.lang.annotation.Annotation`, and javac marks it
    // `ACC_ANNOTATION`. Checking the resolved class rather than "is this a
    // stub?" also rejects a genuinely wrong class of the same name, and states
    // the invariant the proxy's consumers actually depend on.
    is_annotation_type(ctx, resolved).then_some(resolved)
}

/// Is `class_id` an annotation interface (JLS 9.6)?
///
/// `ACC_ANNOTATION` is what javac emits and is the cheap answer. The
/// superinterface check is the same question asked structurally, and covers a
/// class file that carries the interface without the flag.
pub fn is_annotation_type(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    const ACC_ANNOTATION: u16 = 0x2000;
    if ctx.class_access_flags(class_id) & ACC_ANNOTATION != 0 {
        return true;
    }
    ctx.class_interfaces(class_id)
        .into_iter()
        .any(|i| ctx.class_name_arc_of_id(i).as_deref() == Some("java/lang/annotation/Annotation"))
}

/// The IDENTITY half: the declaring class's ("container's") own loader, as
/// HotSpot's `AnnotationParser` does — a child/isolating loader's OWN copy of
/// the annotation interface must back `annotationType()` and `getClass()`
/// (`MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`,
/// `ModifiedClassPathClassLoader`'s child copies of the JAXB API). `None` when
/// there is no user-defined loader, the loader threw (the classloader-isolation
/// filter case), or the mirror does not map back to a ClassId — every one of
/// which leaves the caller on the admission answer.
pub fn resolve_annotation_type_via_container_loader(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    container_loader: Option<ObjectRef>,
) -> Option<ClassId> {
    // `resolve_annotation_class_via_loader` re-enters Java (`loadClass`) and
    // allocates, so the loader reference has to stay rooted across it.
    let loader = container_loader?;
    let loader_pin = ctx.pin_native_root(loader);
    let loader_cur = ctx.read_native_pin(loader_pin, loader);
    let resolved = match resolve_annotation_class_via_loader(ctx, loader_cur, class_name) {
        Ok(mirror) => ctx.class_id_from_mirror(mirror),
        Err(_) => None,
    };
    ctx.unpin_native_roots(loader_pin);
    resolved
}

/// [`create_annotation_proxy_with_type`] for callers that have not already
/// resolved the annotation's type. `None` means the type is unresolvable, in
/// which case NO proxy is built — a proxy with a null `annotationType()` must
/// never reach Java (see [`resolve_annotation_type_class_id`]).
pub fn create_annotation_proxy(
    ctx: &mut dyn NativeContext,
    ann: &cratonvm_native_api::AnnotationData,
    container_class_id: Option<ClassId>,
    container_loader: Option<ObjectRef>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(class_name) = annotation_desc_to_class_name(&ann.type_descriptor) else {
        return Ok(None);
    };
    let owned = class_name.to_string();
    // gen r4w3/rooting: type resolution may load a class (GC point); pin the
    // container loader across it and pass the re-read address on.
    let loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    let resolved = resolve_annotation_type_class_id(ctx, &owned, container_class_id);
    let container_loader = match (container_loader, loader_pin) {
        (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
        _ => None,
    };
    if let Some(pin) = loader_pin {
        ctx.unpin_native_roots(pin);
    }
    let Some(cid) = resolved else {
        return Ok(None);
    };
    Ok(Some(create_annotation_proxy_with_type(
        ctx,
        ann,
        cid,
        container_class_id,
        container_loader,
    )?))
}

/// Create an annotation proxy object from annotation data.
/// Fills in default values for elements not explicitly provided.
///
/// `ann_class_id` is the annotation interface the ADMISSION filter accepted —
/// see [`resolve_annotation_type_class_id`]. The loader-aware lookup still runs
/// here (identity matters: a child loader's own copy must back
/// `annotationType()`), but it can now only REFINE that answer, never replace it
/// with nothing. Before, the builder re-derived the type independently and a
/// failure left the proxy with a null type mirror and — since the
/// `AnnotationDefault` backfill is gated on the same ClassId — with none of its
/// defaulted members filled in.
///
/// `container_loader` is the defining ClassLoader of the class on which the
/// annotation is declared (the "container" in HotSpot's `AnnotationParser`).
/// When present (a user-defined loader), Class-valued members are resolved
/// through it so classloader-isolation patterns are honored; `None` keeps the
/// global resolution used for app/bootstrap-loaded classes.
pub fn create_annotation_proxy_with_type(
    ctx: &mut dyn NativeContext,
    ann: &cratonvm_native_api::AnnotationData,
    ann_class_id: ClassId,
    container_class_id: Option<ClassId>,
    container_loader: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    // Allocating the proxy itself can relocate a user-defined declaring
    // loader before the first loader-aware annotation-type lookup.
    let container_loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    // W7-12/W7-17 — `ensure_vm_internal_class`, not the compatibility door.
    //
    // `java/lang/annotation/AnnotationProxy` is a name NO JDK declares
    // (`javap java.lang.annotation.AnnotationProxy` against the JDK 25 image →
    // "class not found"), so no class file can ever back it. It is the
    // invocation-handler carrier this VM mints for every generated annotation
    // proxy — the exact sibling of `java/lang/reflect/Proxy$Instance` in
    // `reflect_annotations.rs`, which took this same door for this same reason
    // — and contract §1 item 6 lists both among the shapes a conforming JVM
    // creates without a class file. Minted through
    // `try_alloc_concurrent_synthetic` alone it looked like a §5 compatibility
    // stand-in, and `--real-jdk` refused it: measured, that took
    // `getAnnotation` to null on RReflect/RJdkReflect and to
    // `NoClassDefFoundError` on RJdkJmx. The reason ONE cause wore two faces
    // was that the single-annotation entry points below spelled the call
    // `if let Ok(Some(proxy)) = …` and dropped this `?`'s error on the floor;
    // that was residual R1 of the record and is fixed separately under W7-26,
    // so a refusal here now reaches its caller by both routes.
    //
    // The refusal is the ONLY half that moves. `--dump-native-registry` over a
    // boot in each mode reports **zero** natives registered under this class
    // name in Compatible AND in `--real-jdk`, so the second gate a door change
    // has to clear — `register()` re-tagging a receiver's natives
    // `SyntheticStub` and `JdkOnly` then dropping them — has nothing to drop
    // here. That check is per-class and not a general licence: a carrier whose
    // natives ARE dropped in strict gets an `UnsatisfiedLinkError` at its first
    // call instead of a `NoClassDefFoundError` at its mint, which is a moved
    // symptom, not a fix.
    //
    // Pre-mint rather than replace: `fabricate_class` returns the existing
    // `ClassId` for an already-loaded name before it reaches
    // `admit_compatibility_class`, so the allocation below still does its own
    // real-vs-requested field-count widening, its W4-4 layout-alias report and
    // its GC-safe allocation retry, byte-for-byte, in either mode. Only the
    // class's recorded ORIGIN moves.
    ctx.ensure_vm_internal_class("java/lang/annotation/AnnotationProxy", ANN_PROXY_FIELDS);
    let mut proxy = try_alloc_concurrent_synthetic(
        ctx,
        "java/lang/annotation/AnnotationProxy",
        ANN_PROXY_FIELDS,
    )?;
    // GC-safety (2026-07-16): `proxy` is a freshly-allocated object that is
    // not yet reachable from any Java-visible root (it isn't returned to the
    // interpreter until this function's end) -- every allocating call below
    // (`create_string`, class loading/mirror resolution, `declared_methods`,
    // annotation-element conversion) can trigger a GC that relocates it. Pin
    // it for the whole function and re-read the forwarded reference before
    // every use, matching the already-pinned `names_arr`/`values_arr` below.
    let proxy_pin = ctx.pin_native_root(proxy);
    let desc_str = ctx.create_string(&ann.type_descriptor);
    let desc_pin = ctx.pin_native_root(desc_str);
    proxy = ctx.read_native_pin(proxy_pin, proxy);
    let desc_str_cur = ctx.read_native_pin(desc_pin, desc_str);
    ctx.set_field(
        proxy,
        ANN_PROXY_TYPE_DESC,
        Value::Object(Some(desc_str_cur)),
    );
    let desc_str_cur = ctx.read_native_pin(desc_pin, desc_str);
    // gen r4w3/rooting: the child roots are registered at the END, after the
    // element conversion and real-proxy generation (both GC points). Keep each
    // child pinned (released with `proxy_pin`) and re-read them all just
    // before `remember_annotation_proxy_child_roots`.
    let mut child_roots: Vec<(usize, ObjectRef)> = vec![(desc_pin, desc_str_cur)];

    // The type mirror backs `annotation.getClass()` and
    // `annotation.annotationType()`, both of which must report the declaring
    // loader for classloader-isolation patterns
    // (MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader) and
    // both of which callers dereference WITHOUT a null check. Prefer the
    // container loader's own copy; fall back to the admitted ClassId, so the
    // field can never be null.
    let ann_class_id = {
        let loader_cur = match (container_loader, container_loader_pin) {
            (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
            _ => None,
        };
        annotation_desc_to_class_name(&ann.type_descriptor)
            .map(|n| n.to_string())
            .and_then(|name| resolve_annotation_type_via_container_loader(ctx, &name, loader_cur))
            .unwrap_or(ann_class_id)
    };
    let ann_class_id_opt = Some(ann_class_id);
    {
        let mirror = ctx.get_class_mirror(ann_class_id);
        let mirror_pin = ctx.pin_native_root(mirror);
        proxy = ctx.read_native_pin(proxy_pin, proxy);
        let mirror_cur = ctx.read_native_pin(mirror_pin, mirror);
        ctx.set_field(
            proxy,
            ANN_PROXY_TYPE_MIRROR,
            Value::Object(Some(mirror_cur)),
        );
        let mirror_cur = ctx.read_native_pin(mirror_pin, mirror);
        // gen r4w3/rooting: stays pinned; see `child_roots`.
        child_roots.push((mirror_pin, mirror_cur));
    }

    // Collect explicit elements with their declared return-type descriptor
    // (read from the annotation interface's abstract method, when available).
    // The descriptor is used as a fallback hint when the element value is an
    // EMPTY array вЂ” we'd otherwise pick `java/lang/Object` as the component
    // class, which trips Spring's `AnnotationUtils.adaptValue` into treating
    // the array as `Annotation[]` (under the lenient `Object` fallback in
    // `array_is_assignable`) and converts the empty `Object[]` into an empty
    // `AnnotationAttributes[]`.  That collapse later surfaces as
    // `IllegalArgumentException` in `AnnotationAttributes.assertAttributeType`
    // on `getStringArray("pattern")` for `@ComponentScan.Filter` (S111r19).
    let mut all_elements: Vec<(
        String,
        cratonvm_native_api::AnnotationElementValue,
        Option<String>,
        Option<ClassId>,
    )> = ann
        .elements
        .iter()
        .map(|(n, v)| (n.clone(), v.clone(), None, None))
        .collect();

    // Fill in AnnotationDefault values for missing elements
    if let Some(ann_cid) = ann_class_id_opt {
        let methods = ctx.declared_methods(ann_cid);
        let explicit_names: std::collections::HashSet<String> =
            all_elements.iter().map(|(n, _, _, _)| n.clone()).collect();
        // For explicit elements, also backfill the return-type descriptor
        // so empty arrays carry the right component hint.
        for (name, _, desc_slot, _) in all_elements.iter_mut() {
            if let Some(m) = methods.iter().find(|m| &m.name == name) {
                if let Some(ret) = m.descriptor.strip_prefix("()") {
                    *desc_slot = Some(ret.to_string());
                }
            }
        }
        for m in &methods {
            // Annotation elements are abstract no-arg methods
            if explicit_names.contains(&m.name) {
                continue;
            }
            // Skip <init>, <clinit>, etc.
            if m.name.starts_with('<') {
                continue;
            }
            if let Some(default_val) =
                ctx.method_annotation_default(ann_cid, &m.name, &m.descriptor)
            {
                let ret_desc = m.descriptor.strip_prefix("()").map(|s| s.to_string());
                // AnnotationDefault values are resolved in the annotation
                // interface's loader, not the loader of the class carrying
                // this annotation usage. This must agree with
                // Method.getDefaultValue(), which Spring compares against
                // the raw annotation value by Class identity.
                all_elements.push((m.name.clone(), default_val, ret_desc, Some(ann_cid)));
            }
        }
    }

    // ---------------------------------------------------------------------
    // (REMOVED) CGLIB-Оі override that forced `@Configuration(proxyBeanMethods
    // =false)`. That shim put Spring into "lite" mode globally because CratonVM
    // had no CGLIB bytecode-rewriter, so the default "full" semantics (inter-
    // `@Bean`-method calls returning the shared singleton) were not realisable
    // and would otherwise produce duplicate beans.
    //
    // `cglib_enhancer.rs` now emits real `@Bean`-method interception on the
    // enhanced subclass (the `$$beanFactory` field + per-method overrides that
    // route inter-bean references through `beanFactory.getBean`), so full mode
    // works. We therefore let `proxyBeanMethods` resolve to its real annotation
    // value (default `true`) вЂ” Spring enhances the `@Configuration` class and
    // our interceptor gives correct shared-singleton semantics (SB-03 /
    // S03_ConfigProxy).
    // ---------------------------------------------------------------------

    // Store element nameв†’value pairs as parallel arrays
    let n = all_elements.len();
    let mut names_arr = ctx.new_ref_array(ClassId::new(0), n);
    let names_pin = ctx.pin_native_root(names_arr);
    let mut values_arr = ctx.new_ref_array(ClassId::new(0), n);
    let values_pin = ctx.pin_native_root(values_arr);
    // Both arrays are allocated once, then held across a loop whose body
    // (`create_string`, `annotation_element_to_java_typed`) can itself
    // allocate — under the moving collector either array can relocate
    // mid-loop, leaving a stale Rust-local copy; pin both across the whole
    // loop and re-fetch before every use, matching the documented
    // `pin_native_root`/`read_native_pin` contract.
    for (i, (name, val, ret_desc, default_owner)) in all_elements.iter().enumerate() {
        let name_str = ctx.create_string(name);
        let name_pin = ctx.pin_native_root(name_str);
        names_arr = ctx.read_native_pin(names_pin, names_arr);
        let name_cur = ctx.read_native_pin(name_pin, name_str);
        ctx.set_array_element(names_arr, i, Value::Object(Some(name_cur)));
        ctx.unpin_native_roots(name_pin);
        let (value_container_class_id, value_container_loader) = match default_owner {
            Some(owner) => (
                Some(*owner),
                crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), owner.as_u32()),
            ),
            None => (
                container_class_id,
                match (container_loader, container_loader_pin) {
                    (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                    _ => None,                },
            ),
        };
        let java_val = annotation_element_to_java_typed(
            ctx,
            val,
            ret_desc.as_deref(),
            value_container_class_id,
            value_container_loader,
            ann_class_id_opt,
        )?;
        if crate::nbflags().iae_trace2 {
            let desc = match &java_val {
                Value::Object(Some(o)) => {
                    let cid = ctx.class_id_of_object(*o);
                    let is_arr = ctx.heap_kind_of(*o) == cratonvm_types::ObjectKind::Array;
                    let len = if is_arr {
                        ctx.array_length(*o) as i64
                    } else {
                        -1
                    };
                    format!(
                        "Object(cid={} name={:?} is_array={} len={})",
                        cid.as_u32(),
                        ctx.class_name_of_id(cid),
                        is_arr,
                        len
                    )
                }
                Value::Object(None) => "Object(null)".to_string(),
                other => format!("{other:?}"),
            };
            eprintln!(
                "ANN-ELEM holder={} name={} value={}",
                ann.type_descriptor, name, desc
            );
        }
        values_arr = ctx.read_native_pin(values_pin, values_arr);
        let java_val_pin = match java_val {
            Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
            _ => None,
        };
        let java_val_cur = match (java_val, java_val_pin) {
            (Value::Object(Some(object)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, object)))
            }
            _ => java_val,
        };
        ctx.set_array_element(values_arr, i, java_val_cur);
        if let Some(pin) = java_val_pin {
            ctx.unpin_native_roots(pin);
        }
    }
    names_arr = ctx.read_native_pin(names_pin, names_arr);
    values_arr = ctx.read_native_pin(values_pin, values_arr);
    proxy = ctx.read_native_pin(proxy_pin, proxy);
    ctx.set_field(proxy, ANN_PROXY_ELEM_NAMES, Value::Object(Some(names_arr)));
    proxy = ctx.read_native_pin(proxy_pin, proxy);
    values_arr = ctx.read_native_pin(values_pin, values_arr);
    ctx.set_field(
        proxy,
        ANN_PROXY_ELEM_VALUES,
        Value::Object(Some(values_arr)),
    );
    names_arr = ctx.read_native_pin(names_pin, names_arr);
    values_arr = ctx.read_native_pin(values_pin, values_arr);
    // gen r4w3/rooting: both arrays stay pinned (released with `proxy_pin`)
    // until the child roots are re-read below.
    child_roots.push((names_pin, names_arr));
    child_roots.push((values_pin, values_arr));

    // Real annotations: hand back a real `$ProxyN` proxy that wraps
    // this AnnotationProxy as its InvocationHandler (so `getClass()` is a
    // `$ProxyN`). Falls back to the bare AnnotationProxy when generation fails.
    // A real proxy needs the VM-owned loader and generated-class namespace.
    // The lightweight native unit-test context deliberately does not model
    // that global state, so retain the valid AnnotationProxy representation
    // there instead of reusing a loader ObjectRef from another mock VM.
    proxy = ctx.read_native_pin(proxy_pin, proxy);
    if ctx.supports_real_proxy_generation() && real_annotations_enabled() {
        if let Some(ann_cid) = ann_class_id_opt {
            if let Some(real) = wrap_annotation_in_real_proxy(ctx, ann_cid, proxy) {
                ctx.unpin_native_roots(proxy_pin);
                if let Some(pin) = container_loader_pin {
                    ctx.unpin_native_roots(pin);
                }
                return Ok(real);
            }
            proxy = ctx.read_native_pin(proxy_pin, proxy);
        }
    }
    // gen r4w3/rooting: current addresses of every child, read after the last
    // GC point and before the pins go.
    let child_roots: Vec<ObjectRef> = child_roots
        .iter()
        .map(|(pin, obj)| ctx.read_native_pin(*pin, *obj))
        .collect();
    ctx.unpin_native_roots(proxy_pin);
    remember_annotation_proxy_child_roots(ctx.vm_identity(), proxy, child_roots);
    if let Some(pin) = container_loader_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(proxy)
}

/// Convert an AnnotationElementValue to a Java Value.
///
/// WP2.7: primitives are boxed into the **correct wrapper class** (not
/// `java.lang.Object`) so spec-compliant `Annotation.equals` can identify
/// them as wrappers and compare by underlying value, and so the proxy's
/// element accessor methods return wrappers that pass downstream
/// `instanceof Integer` checks.
pub fn annotation_element_to_java(
    ctx: &mut dyn NativeContext,
    val: &cratonvm_native_api::AnnotationElementValue,
) -> Result<Value, MethodCallFailed> {
    Ok(annotation_element_to_java_typed(
        ctx, val, None, None, None, None,
    )?)
}

/// Preserve the declared array shape when the class-file annotation reader
/// reports a lone nested annotation as a scalar element. Java permits the
/// shorthand `member = @Nested(...)` for a `Nested[]` member, but consumers
/// such as Spring still receive the declared array type at runtime.
pub fn normalize_single_annotation_array(
    ctx: &mut dyn NativeContext,
    value: Value,
    return_type_desc: Option<&str>,
) -> Value {
    let Some(component_name) = return_type_desc
        .and_then(|desc| desc.strip_prefix("[L"))
        .and_then(|desc| desc.strip_suffix(';'))
    else {
        return value;
    };
    let component_id = ctx.class_id_by_name(component_name).or_else(|| {
        let _ = ctx.load_class(component_name);
        ctx.class_id_by_name(component_name)
    });
    let Some(component_id) = component_id else {
        return value;
    };
    let value_pin = match value {
        Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
        _ => None,
    };
    let array = ctx.new_ref_array(component_id, 1);
    let array_pin = ctx.pin_native_root(array);
    let element = match (value, value_pin) {
        (Value::Object(Some(object)), Some(pin)) => {
            Value::Object(Some(ctx.read_native_pin(pin, object)))
        }
        (other, _) => other,
    };
    let array_cur = ctx.read_native_pin(array_pin, array);
    ctx.set_array_element(array_cur, 0, element);
    let array_cur = ctx.read_native_pin(array_pin, array);
    if let Some(pin) = value_pin {
        ctx.unpin_native_roots(pin);
    } else {
        ctx.unpin_native_roots(array_pin);
    }
    Value::Object(Some(array_cur))
}

pub fn boxed_annotation_primitive(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    value: Value,
) -> Result<Value, MethodCallFailed> {
    let object = crate::try_alloc_concurrent_synthetic(ctx, class_name, 1)?;
    let object_pin = ctx.pin_native_root(object);
    let object_cur = ctx.read_native_pin(object_pin, object);
    ctx.set_field(object_cur, 0, value);
    let object_cur = ctx.read_native_pin(object_pin, object);
    ctx.unpin_native_roots(object_pin);
    Ok(Value::Object(Some(object_cur)))
}

/// S111r19 вЂ” typed variant: when called for a known annotation-element method,
/// the caller passes the method's return-type descriptor (e.g.
/// `[Ljava/lang/String;`).  Used to recover the array component class for
/// **empty** array values, which would otherwise default to
/// `java/lang/Object` and trip Spring's `AnnotationUtils.adaptValue` into
/// converting the empty `Object[]` to an empty `AnnotationAttributes[]`
/// (because our `array_is_assignable` lenient `Object` fallback green-lights
/// `Annotation[].isInstance(Object[])`).
pub fn annotation_element_to_java_typed(
    ctx: &mut dyn NativeContext,
    val: &cratonvm_native_api::AnnotationElementValue,
    return_type_desc: Option<&str>,
    container_class_id: Option<ClassId>,
    container_loader: Option<ObjectRef>,
    annotation_class_id: Option<ClassId>,
) -> Result<Value, MethodCallFailed> {
    use cratonvm_native_api::AnnotationElementValue;
    let container_loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    let result = (|| -> Result<Value, MethodCallFailed> {
        match val {
            AnnotationElementValue::Int(v) => {
                // Round 18 fix: `AnnotationElementValue::Int` is overloaded for
                // boolean/byte/char/short/int (the `.class` AnnotationDefault
                // attribute encodes Z/B/C/S/I tags as int constants in the CP).
                // When the caller knows the annotation method's return-type
                // descriptor we must box into the matching wrapper, else
                // Spring's `TypeMappedAnnotation.adapt` rejects e.g.
                // `proxyBeanMethods` (declared `boolean`) when given an Integer
                // (`should be compatible with java.lang.Boolean but a
                // java.lang.Integer value was returned`), causing
                // `ConfigurationClassParser.processImports` to silently drop the
                // `@Import(AutoConfigurationImportSelector.class)` directive on
                // `@SpringBootApplication` and ultimately surfacing as
                // `MissingWebServerFactoryBeanException`.
                let (wrapper, value) = match return_type_desc {
                    Some("Z") => ("java/lang/Boolean", Value::Int(if *v != 0 { 1 } else { 0 })),
                    Some("B") => ("java/lang/Byte", Value::Int(*v as i8 as i32)),
                    Some("C") => ("java/lang/Character", Value::Int(*v & 0xFFFF)),
                    Some("S") => ("java/lang/Short", Value::Int(*v as i16 as i32)),
                    _ => ("java/lang/Integer", Value::Int(*v)),
                };
                boxed_annotation_primitive(ctx, wrapper, value)
            }
            AnnotationElementValue::Long(v) => {
                boxed_annotation_primitive(ctx, "java/lang/Long", Value::Long(*v))
            }
            AnnotationElementValue::Float(v) => {
                boxed_annotation_primitive(ctx, "java/lang/Float", Value::Float(*v))
            }
            AnnotationElementValue::Double(v) => {
                boxed_annotation_primitive(ctx, "java/lang/Double", Value::Double(*v))
            }
            AnnotationElementValue::StringVal(s) => {
                let str_obj = ctx.create_string(s);
                Ok(Value::Object(Some(str_obj)))
            }
            AnnotationElementValue::Enum(type_desc, const_name) => {
                // Resolve the enum class from the type descriptor and create the constant.
                // type_desc is like "Ljava/lang/annotation/RetentionPolicy;" вЂ” strip L and ;
                let class_name = type_desc
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .unwrap_or(type_desc);
                // S111r19 вЂ” load the enum class on demand if not yet loaded.
                // Annotation proxies are materialised eagerly during the
                // declaring class's load, but the enum class referenced by the
                // annotation's element values (e.g. `FilterType` in
                // `@ComponentScan.Filter.type`) often is **not** yet loaded.
                // Previously we fell straight through to the synthetic fallback
                // which writes ordinal=0 вЂ” collapsing `FilterType.CUSTOM`
                // (real ordinal 4) onto `ANNOTATION` (ordinal 0) and sending
                // Spring's `ComponentScanAnnotationParser.typeFiltersFor` into
                // the wrong switch case, surfacing as `IllegalArgumentException`
                // wrapped at `ConfigurationClassParser.parse:181`.  Load the
                // class on demand, mirroring the sibling `Class` arm (C29).
                //
                // Loader-faithful resolution (found via
                // `SpringBootContextLoaderAotTests`, `@CompileWithForkedClassLoader`):
                // `ctx.class_id_by_name` is a GLOBAL "one class per name" lookup.
                // Under a forked/isolating classloader, the annotation's declaring
                // class (and the bytecode that later compares this default value
                // via `==`, e.g. `useMainMethod == UseMainMethod.NEVER` in
                // `SpringBootContextLoader.getMainMethod`) is loaded by the FORKED
                // loader, but the global table can still resolve `class_name` to
                // the outer/app loader's copy of the enum class — producing a
                // same-named but reference-UNEQUAL enum constant (default
                // `UseMainMethod.NEVER` from the wrong loader), so the `==` check
                // silently fails and `useMainMethod` behaves as if it were
                // `ALWAYS` (ordinal 0). Mirror the `Class`-valued arm above: when
                // `container_loader` is present, resolve the enum type through it
                // first via `loadClass`, so the SAME loader's copy backs both the
                // default value and the bytecode's own reference to the constant.
                let iae_trace = crate::nbflags().iae_trace_ok;
                let loader_cur = match (container_loader, container_loader_pin) {
                    (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                    _ => None,
                };
                let via_loader = loader_cur.and_then(|loader| {
                    match resolve_annotation_class_via_loader(ctx, loader, class_name) {
                        Ok(mirror) => ctx.class_id_from_mirror(mirror),
                        Err(_) => None,
                    }
                });
                let annotation_scope = annotation_class_id.and_then(|annotation_class| {
                    ctx.class_id_by_name_near(class_name, annotation_class)
                });
                // An application-loaded annotation can lack an ObjectRef for its
                // defining loader while its ClassId still carries the correct
                // namespace. Prefer that scoped lookup to the global table.
                let via_container_scope = container_class_id
                    .and_then(|holder| ctx.class_id_by_name_near(class_name, holder));
                let enum_cid_opt = annotation_scope
                    .or(via_loader)
                    .or(via_container_scope)
                    .or_else(|| {
                        // Resolve an annotation enum reference through the declaring
                        // member's initiating loader. A bare global lookup can miss an
                        // application dependency that is visible to that member (for
                        // example Mockito's `Mock$Strictness`) and can also lose the
                        // correct identity under an isolated loader.
                        container_class_id.and_then(|holder| {
                            ctx.class_id_by_name_via_referencing_class(holder, class_name)
                                .ok()
                        })
                    })
                    .or_else(|| {
                        ctx.class_id_by_name(class_name).or_else(|| {
                            let _ = ctx.load_class(class_name);
                            ctx.class_id_by_name(class_name)
                        })
                    });
                if let Some(enum_cid) = enum_cid_opt {
                    // GC-safety (2026-07-16): this is the "enum builder" residual
                    // gap flagged (but never swept) in
                    // jit-junit-discovery-reflection-corruption.md
                    // — `class_mirror` is held in a Rust local across the
                    // allocating `create_string` call (and the `Enum.valueOf`
                    // invocation itself, which can allocate/classload) before
                    // being used as an invoke argument. Pin it and re-read the
                    // forwarded reference right before use.
                    let class_mirror = ctx.get_class_mirror(enum_cid);
                    let class_mirror_pin = ctx.pin_native_root(class_mirror);
                    let name_str = ctx.create_string(const_name);
                    let name_pin = ctx.pin_native_root(name_str);
                    let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
                    let name_str = ctx.read_native_pin(name_pin, name_str);
                    let invoke_res = ctx.invoke(
                        "java/lang/Enum",
                        "valueOf",
                        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;",
                        &[
                            Value::Object(Some(class_mirror)),
                            Value::Object(Some(name_str)),
                        ],
                    );
                    ctx.unpin_native_roots(class_mirror_pin);
                    if iae_trace {
                        eprintln!(
                            "ANN-ENUM class={class_name} const={const_name} ok={}",
                            invoke_res.as_ref().map(|v| v.is_some()).unwrap_or(false)
                        );
                    }
                    if let Ok(Some(val)) = invoke_res {
                        return Ok(val);
                    }
                } else if iae_trace {
                    eprintln!("ANN-ENUM class={class_name} const={const_name} CLASS-NOT-FOUND");
                }
                // Fallback: allocate a synthetic enum instance with the name and ordinal
                if iae_trace {
                    eprintln!("ANN-ENUM FALLBACK class={class_name} const={const_name} ordinal=0");
                }
                let obj = try_alloc_concurrent_synthetic(ctx, class_name, 2)?;
                let obj_pin = ctx.pin_native_root(obj);
                let name_str = ctx.create_string(const_name);
                let name_pin = ctx.pin_native_root(name_str);
                let obj_cur = ctx.read_native_pin(obj_pin, obj);
                let name_cur = ctx.read_native_pin(name_pin, name_str);
                ctx.set_field(obj_cur, 0, Value::Object(Some(name_cur)));
                let obj_cur = ctx.read_native_pin(obj_pin, obj);
                ctx.set_field(obj_cur, 1, Value::Int(0)); // ordinal
                let obj_cur = ctx.read_native_pin(obj_pin, obj);
                ctx.unpin_native_roots(obj_pin);
                Ok(Value::Object(Some(obj_cur)))
            }
            AnnotationElementValue::Class(desc) => {
                // Return Class mirror. If the target class is not yet loaded
                // (common for annotation defaults that reference sibling classes
                // like picocli's NoOpModelTransformer), load it on demand. A null
                // return here causes downstream NullPointerExceptions (C29).
                let iae_trace_cls = crate::nbflags().iae_trace_ok;
                if let Some(class_name) = annotation_desc_to_class_name(desc) {
                    // Classloader-isolation: when the declaring class was loaded by a
                    // user-defined loader, resolve the Class member THROUGH that loader
                    // (HotSpot's `AnnotationParser.parseClassValue(sig, container)`). A
                    // `ClassNotFoundException` becomes a deferred `TypeNotPresentException`
                    // (stored as the member value, thrown at access). This honors a
                    // FilteringClassLoader that rejects the referenced type; without it
                    // the global resolve below would silently return the app-loaded
                    // class and the filter would be bypassed.
                    let loader_cur = match (container_loader, container_loader_pin) {
                        (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                        _ => None,
                    };
                    if let Some(loader) = loader_cur {
                        let owned = class_name.to_string();
                        match resolve_annotation_class_via_loader(ctx, loader, &owned) {
                            Ok(mirror) => {
                                if iae_trace_cls {
                                    eprintln!(
                                    "ANN-CLASS desc={desc} class={owned} via-container-loader ok"
                                );
                                }
                                return Ok(Value::Object(Some(mirror)));
                            }
                            Err(Some(cnfe)) => {
                                if iae_trace_cls {
                                    eprintln!("ANN-CLASS desc={desc} class={owned} container-loader CNFE -> TypeNotPresentException");
                                }
                                if let Some(tnpe) = make_type_not_present_exception(
                                    ctx,
                                    &owned.replace('/', "."),
                                    Some(cnfe),
                                    true,
                                ) {
                                    return Ok(Value::Object(Some(tnpe)));
                                }
                                // Could not build the sentinel вЂ” fall through to global.
                            }
                            Err(None) => {
                                // Loader returned null / internal error вЂ” fall through
                                // to the global resolution below (best-effort).
                            }
                        }
                    }
                    let scoped = container_class_id
                        .and_then(|holder| ctx.class_id_by_name_near(class_name, holder));
                    // A `Class`-valued member names a type the CONTAINER refers to,
                    // so it resolves with the container's initiating loader
                    // (HotSpot: `parseClassValue` -> `Class.forName(n, false,
                    // container.getClassLoader())`). `class_id_by_name_near` covers
                    // that loader's own namespace and the built-in delegation chain
                    // it inherits, but only for names ALREADY resolved there;
                    // driving the container's loader covers the first use too. The
                    // sibling `Enum` arm has taken that step since the
                    // `SpringBootContextLoaderAotTests` fix — this arm had not, and
                    // fell straight through to the loader-BLIND `class_id_by_name`
                    // instead.
                    //
                    // That fallback answers with whatever single loader happens to
                    // have the name, which under `@CompileWithForkedClassLoader` is
                    // the FORK: an Application-loaded log4j `@PluginAttribute`
                    // (loader 2) was handed the fork's (loader 3)
                    // `PluginAttributeVisitor`, and every class reached from there
                    // — `AbstractPluginVisitor`, `TypeConverters`,
                    // `TypeConverterRegistry` — was forked too, so an app-world
                    // `ConfigurationStrSubstitutor` could not be cast to the
                    // visitor's fork-world `StrSubstitutor`. log4j then dropped
                    // every `<Logger>` element of its configuration.
                    let driven = scoped
                        .is_none()
                        .then(|| {
                            container_class_id.and_then(|holder| {
                                ctx.class_id_by_name_via_referencing_class(holder, class_name)
                                    .ok()
                            })
                        })
                        .flatten();
                    if let Some(cid) = scoped
                        .or(driven)
                        .or_else(|| ctx.class_id_by_name(class_name))
                    {
                        let mirror = ctx.get_class_mirror(cid);
                        if iae_trace_cls {
                            eprintln!(
                                "ANN-CLASS desc={desc} class={class_name} already-loaded ok \
                             holder={:?}/L{:?} via={} answer=L{}",
                                container_class_id.map(|h| h.as_u32()),
                                container_class_id.map(|h| ctx.loader_id_of_class(h)),
                                if scoped.is_some() {
                                    "scoped"
                                } else if driven.is_some() {
                                    "container-loader"
                                } else {
                                    "GLOBAL"
                                },
                                ctx.loader_id_of_class(cid),
                            );
                        }
                        return Ok(Value::Object(Some(mirror)));
                    }
                    let load_res = ctx.load_class(class_name);
                    if iae_trace_cls {
                        eprintln!(
                            "ANN-CLASS desc={desc} class={class_name} load-ok={}",
                            load_res.as_ref().map(|v| v.is_some()).unwrap_or(false)
                        );
                    }
                    if let Ok(Some(val)) = load_res {
                        return Ok(val);
                    }
                    // Genuinely unresolvable (no container_loader took the CNFE
                    // branch above, e.g. plain app/bootstrap-loaded classes, which
                    // is the overwhelmingly common case for `@ConditionalOnClass`
                    // referencing an optional dependency). Mirror HotSpot's
                    // `AnnotationParser.parseClassValue`: a ClassNotFoundException
                    // here becomes a deferred `TypeNotPresentException` sentinel
                    // (thrown on member ACCESS by `annotation_proxy_dispatch_impl`),
                    // never a bare Java `null`. Handing back `null` let Spring's
                    // `TypeMappedAnnotation`/`MergedAnnotation` `classValuesAsString`
                    // conversion (`Class.getName()` on the null element) raise an
                    // unexpected `NullPointerException` that Spring's
                    // `TypeNotPresentException`-aware handling for exactly this
                    // optional-dependency pattern doesn't recognize — surfacing as
                    // `OnClassCondition.addAll`'s "NullPointerException cannot be
                    // cast to String[]" across every `@ConditionalOnClass`-gated
                    // autoconfiguration whose referenced class is absent.
                    if let Some(tnpe) = shared_unresolvable_class_sentinel(ctx) {
                        if iae_trace_cls {
                            eprintln!(
                            "ANN-CLASS desc={desc} class={class_name} unresolved -> TypeNotPresentException (shared)"
                        );
                        }
                        return Ok(Value::Object(Some(tnpe)));
                    }
                    if iae_trace_cls {
                        eprintln!("ANN-CLASS desc={desc} class={class_name} RETURNING-NULL");
                    }
                    // Could not even build the sentinel (e.g. TypeNotPresentException
                    // itself isn't loadable) вЂ” preserve the prior best-effort null.
                    return Ok(Value::Object(None));
                }
                // `annotation_desc_to_class_name` returned None: the descriptor is
                // a primitive (`I`/`J`/...), `void` (`V`), or an array (`[...`) вЂ”
                // most notably `default void.class`, used by ByteBuddy's
                // `@Advice.FieldValue.declaringType()`. Returning null here made the
                // annotation member read back as null, so ByteBuddy's
                // `declaringType.represents(void.class)` NPE'd (`getName()` on a
                // null TypeDescription) inside Hibernate's BytecodeProvider init.
                // `descriptor_to_class_mirror` maps `V` -> the `void` primitive
                // Class mirror, `[I` -> the canonical `int[]` mirror, etc.
                if iae_trace_cls {
                    eprintln!(
                        "ANN-CLASS desc={desc} primitive/void/array -> descriptor_to_class_mirror"
                    );
                }
                Ok(Value::Object(Some(descriptor_to_class_mirror(ctx, desc))))
            }
            AnnotationElementValue::Annotation(nested) => {
                let loader_cur = match (container_loader, container_loader_pin) {
                    (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                    _ => None,
                };
                Ok(
                    match create_annotation_proxy(ctx, nested, container_class_id, loader_cur)? {
                        Some(proxy) => normalize_single_annotation_array(
                            ctx,
                            Value::Object(Some(proxy)),
                            return_type_desc,
                        ),
                        // The nested annotation's own type is unresolvable. This arm
                        // used to hand back a proxy with a null `annotationType()` —
                        // unlike the top-level array builders, it never consulted the
                        // loadability filter. Store the deferred sentinel the
                        // Class-valued arm already uses: the member THROWS on access
                        // (HotSpot fails harder still — `getDeclaredAnnotations()`
                        // itself raises NoClassDefFoundError for the unresolvable
                        // element type), and no null-typed annotation escapes.
                        None => match unresolvable_nested_annotation_sentinel(ctx, nested) {
                            Some(sentinel) => Value::Object(Some(sentinel)),
                            None => Value::Object(None),
                        },
                    },
                )
            }
            AnnotationElementValue::Array(elems) => {
                // Pick a component class for the array based on the element kind so
                // downstream `instanceof "[Lfoo;"` checks correctly distinguish
                // between e.g. `String[]` and `Annotation[]`. Spring's
                // `AnnotationUtils.adaptValue` runs an `instanceof
                // "[Ljava/lang/annotation/Annotation;"` chain вЂ” if the array's
                // component class is bare `Object` (cid=0), the lenient
                // assignability fallback in `array_is_assignable_to`
                // (interpreter.rs `if src_comp == "java/lang/Object" { return
                // true; }`) green-lights the cast and a `String[]` flows into the
                // `Annotation[]` branch, eventually surfacing as
                // `String.annotationType()` NSME inside
                // `retrieveAnnotationAttributes`.
                //
                // Pick by inspecting the first element variant вЂ” annotation
                // attribute arrays are homogeneous per JLS В§9.6.1.
                //
                // S111r18 вЂ” for nested-annotation arrays, use the annotation
                // interface type (e.g. `F4` for `@CScan(excludeFilters=@F4...)`)
                // as the component class, NOT the bare `AnnotationProxy` synthetic.
                // Spring's `MergedAnnotation.adaptForAttribute` walks
                // `returnType.componentType().isAnnotation()` вЂ” when our array
                // reports its component as `AnnotationProxy` (which is
                // `isAnnotation()=false`), the adapt-array branch is taken on
                // returnType but the receiving array allocation in the same
                // method later fails its checkcast / isInstance check, and the
                // built `MergedAnnotation[]` collapses into a single-element
                // value path that surfaces in `AnnotationAttributes` as
                // `[null]`. Routing the component class to the actual annotation
                // interface (F4) makes the array's `componentType()` report
                // `F4.class`, which `isAnnotation()` returns `true` for, and the
                // synthesize loop then runs as expected.
                use cratonvm_native_api::AnnotationElementValue as AEV;
                // SB-02b вЂ” primitive annotation arrays (`int[] mv()`, `boolean[]`,
                // `long[]`, вЂ¦) must materialise as REAL primitive arrays, not boxed
                // wrapper arrays. The canonical tripwire is `@kotlin.Metadata.mv()`
                // (the metadata version, declared `int[]`): kotlin-reflect reads it
                // as a primitive `int[]` (`iaload`), so a boxed `Integer[]` reads
                // back as all-zeros в†’ version parses as `(0,0,0)` в†’ kotlin-reflect
                // treats the class metadata as invalid/legacy and falls back to
                // Java-reflection platform types. That silently corrupts every
                // Kotlin reflective query (return-type nullability, `isSuspend`,
                // value-parameter default-value flags, continuation-parameter
                // hiding), breaking all Spring Kotlin metadata tests while HotSpot
                // passes. When the method return type is a single-dimension
                // primitive array, allocate the matching primitive array and store
                // unboxed values; the boxed-wrapper path below is for reference
                // (`String[]`, `Class[]`, `Annotation[]`, enum) arrays only.
                if let Some(comp) = return_type_desc.and_then(|rd| rd.strip_prefix('[')) {
                    use cratonvm_types::ArrayElementType as AET;
                    let prim = match comp {
                        "Z" => Some(AET::Boolean),
                        "B" => Some(AET::Byte),
                        "C" => Some(AET::Char),
                        "S" => Some(AET::Short),
                        "I" => Some(AET::Int),
                        "J" => Some(AET::Long),
                        "F" => Some(AET::Float),
                        "D" => Some(AET::Double),
                        _ => None,
                    };
                    if let Some(et) = prim {
                        let arr = ctx.new_array(et, elems.len());
                        let arr_pin = ctx.pin_native_root(arr);
                        for (i, elem) in elems.iter().enumerate() {
                            // `AnnotationElementValue::Int` is overloaded for
                            // Z/B/C/S/I (the CP encodes them all as int constants),
                            // so narrow per the descriptor; J/F/D carry their own
                            // variants. A kind mismatch yields a typed zero so the
                            // array stays machine-well-formed.
                            let pv = match (comp, elem) {
                                ("Z", AEV::Int(v)) => Value::Int(if *v != 0 { 1 } else { 0 }),
                                ("B", AEV::Int(v)) => Value::Int(*v as i8 as i32),
                                ("C", AEV::Int(v)) => Value::Int(*v & 0xFFFF),
                                ("S", AEV::Int(v)) => Value::Int(*v as i16 as i32),
                                ("I", AEV::Int(v)) => Value::Int(*v),
                                ("J", AEV::Long(v)) => Value::Long(*v),
                                ("F", AEV::Float(v)) => Value::Float(*v),
                                ("D", AEV::Double(v)) => Value::Double(*v),
                                (_, _) => match et {
                                    AET::Long => Value::Long(0),
                                    AET::Float => Value::Float(0.0),
                                    AET::Double => Value::Double(0.0),
                                    _ => Value::Int(0),
                                },
                            };
                            let arr_cur = ctx.read_native_pin(arr_pin, arr);
                            ctx.set_array_element(arr_cur, i, pv);
                        }
                        let arr_cur = ctx.read_native_pin(arr_pin, arr);
                        ctx.unpin_native_roots(arr_pin);
                        return Ok(Value::Object(Some(arr_cur)));
                    }
                }
                // S111r19 вЂ” when the array is **empty** (no first element to
                // probe), fall back to the caller-provided method return-type
                // descriptor.  This recovers the right component class for
                // empty `String[]` / `Class[]` defaults like `@Filter.pattern()`
                // = `{}`, which would otherwise become an `Object[]` and trip
                // Spring's `AnnotationUtils.adaptValue` Annotation[]-detection
                // (the lenient `Object` fallback in `array_is_assignable`).
                let comp_name_owned: String = match elems.first() {
                    Some(AEV::StringVal(_)) => "java/lang/String".to_string(),
                    Some(AEV::Class(_)) => "java/lang/Class".to_string(),
                    Some(AEV::Annotation(nested)) => {
                        annotation_desc_to_class_name(&nested.type_descriptor)
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "java/lang/annotation/AnnotationProxy".to_string())
                    }
                    Some(AEV::Enum(type_desc, _)) => type_desc
                        .strip_prefix('L')
                        .and_then(|s| s.strip_suffix(';'))
                        .unwrap_or("java/lang/Enum")
                        .to_string(),
                    // Primitive arrays in annotations (`int[]`, `boolean[]`, etc.)
                    // are still allocated as boxed wrapper arrays here per
                    // pre-existing behaviour вЂ” pick the wrapper class.
                    //
                    // spring-bug-01: AEV::Int is overloaded for Z/B/C/S/I (the
                    // `.class` AnnotationDefault encodes them all as int constants).
                    // The element values below are already boxed into the correct
                    // wrapper via `elem_desc`, but the ARRAY's component class was
                    // hardcoded to Integer here вЂ” so a `char[]` default became an
                    // `Integer[]`-typed array holding Character values, and Spring's
                    // `AnnotationTypeMapping.adapt` rejected it ("should be compatible
                    // with char[] but a java.lang.Integer[] value was returned").
                    // Derive the component wrapper from the array's component
                    // descriptor so `[C`в†’Character[], `[Z`в†’Boolean[], etc. (which
                    // Spring then coerces to the primitive array).
                    Some(AEV::Int(_)) => match return_type_desc.and_then(|rd| rd.strip_prefix('['))
                    {
                        Some("Z") => "java/lang/Boolean".to_string(),
                        Some("B") => "java/lang/Byte".to_string(),
                        Some("C") => "java/lang/Character".to_string(),
                        Some("S") => "java/lang/Short".to_string(),
                        _ => "java/lang/Integer".to_string(),
                    },
                    Some(AEV::Long(_)) => "java/lang/Long".to_string(),
                    Some(AEV::Float(_)) => "java/lang/Float".to_string(),
                    Some(AEV::Double(_)) => "java/lang/Double".to_string(),
                    None => {
                        // Empty array вЂ” derive component from method return type.
                        if let Some(rd) = return_type_desc {
                            if let Some(comp) = rd.strip_prefix('[') {
                                if let Some(stripped) =
                                    comp.strip_prefix('L').and_then(|s| s.strip_suffix(';'))
                                {
                                    stripped.to_string()
                                } else if comp.len() == 1 && "ZBCSIJFD".contains(&comp[..1]) {
                                    // Empty primitive array вЂ” boxed wrapper
                                    // (matches the non-empty primitive arms).
                                    match &comp[..1] {
                                        "Z" => "java/lang/Boolean".to_string(),
                                        "B" => "java/lang/Byte".to_string(),
                                        "C" => "java/lang/Character".to_string(),
                                        "S" => "java/lang/Short".to_string(),
                                        "I" => "java/lang/Integer".to_string(),
                                        "J" => "java/lang/Long".to_string(),
                                        "F" => "java/lang/Float".to_string(),
                                        "D" => "java/lang/Double".to_string(),
                                        _ => "java/lang/Object".to_string(),
                                    }
                                } else {
                                    "java/lang/Object".to_string()
                                }
                            } else {
                                "java/lang/Object".to_string()
                            }
                        } else {
                            "java/lang/Object".to_string()
                        }
                    }
                    _ => "java/lang/Object".to_string(),
                };
                // Resolve the COMPONENT through the declaring class's loader first,
                // exactly as the scalar `Enum` and `Class` arms above already do.
                // `class_id_by_name` is the loader-blind global lookup, so under
                // classloader isolation the array's component came from the app
                // loader while the attribute's declared return type came from the
                // fork -- and Spring's `AnnotationTypeMapping.adapt` rejected the
                // value with the self-contradictory "should be compatible with
                // RequestMethod[] but a RequestMethod[] value was returned"
                // (`test.context.aot.TestContextAotGeneratorIntegrationTests`,
                // `test.context.aot.AotIntegrationTests`). `probes/ForkArrProbe.java`.
                let loader_cur = match (container_loader, container_loader_pin) {
                    (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                    _ => None,
                };
                let comp_cid = loader_cur
                    .and_then(|loader| {
                        match resolve_annotation_class_via_loader(ctx, loader, &comp_name_owned) {
                            Ok(mirror) => ctx.class_id_from_mirror(mirror),
                            Err(_) => None,
                        }
                    })
                    .or_else(|| {
                        container_class_id
                            .and_then(|holder| ctx.class_id_by_name_near(&comp_name_owned, holder))
                    })
                    // Same step the scalar `Class` arm takes: `class_id_by_name_near`
                    // only sees names the container's loader has ALREADY resolved,
                    // so drive that loader before conceding to the loader-blind
                    // lookup below — otherwise a component this container has never
                    // touched is answered by whichever single loader happens to
                    // have it.
                    .or_else(|| {
                        container_class_id.and_then(|holder| {
                            ctx.class_id_by_name_via_referencing_class(holder, &comp_name_owned)
                                .ok()
                        })
                    })
                    .or_else(|| ctx.class_id_by_name(&comp_name_owned))
                    .or_else(|| {
                        let _ = ctx.load_class(&comp_name_owned);
                        ctx.class_id_by_name(&comp_name_owned)
                    })
                    .unwrap_or(cratonvm_types::ClassId::new(0));
                let arr = ctx.new_ref_array(comp_cid, elems.len());
                // `arr` is allocated once, then held across a loop whose body
                // (the recursive `annotation_element_to_java_typed` call below)
                // can itself allocate — nested `String`/`Class`/nested-annotation
                // elements always could, and an unresolvable `Class` element now
                // can too (`shared_unresolvable_class_sentinel`, first call ever
                // per process). Under the moving collector `arr` can relocate
                // mid-loop, leaving this Rust-local copy stale; pin it across the
                // whole loop and re-fetch on every iteration, matching the
                // documented `pin_native_root`/`read_native_pin` contract.
                let arr_pin = ctx.pin_native_root(arr);
                // Round 18: derive the per-element return-type descriptor from
                // the array descriptor (strip leading `[`) so primitive elements
                // box into the correct wrapper (Z/B/C/S в†’ Boolean/Byte/Char/Short
                // instead of always Integer).
                let elem_desc: Option<String> = return_type_desc
                    .and_then(|rd| rd.strip_prefix('['))
                    .map(|s| s.to_string());
                // `Class[]`-declared members (e.g. `@ConditionalOnClass`'s `value`)
                // resolve each element independently; an unresolvable class yields
                // a per-element `TypeNotPresentException` sentinel (see the
                // `Class(desc)` arm above). HotSpot's own `AnnotationInvocationHandler
                // .invoke` walks such a member's backing array and throws on the
                // FIRST sentinel found for the WHOLE accessor call — it never hands
                // the array itself back to the caller. Mirror that here at
                // construction time (once per annotation instance) rather than on
                // every subsequent access: collapse the whole member to the
                // sentinel the moment one turns up, matching the existing scalar
                // `Class`-member sentinel representation that
                // `annotation_proxy_dispatch_impl` already knows how to throw.
                // Without this, Spring's own `TypeNotPresentException`-aware
                // attribute extraction (built specifically to catch this from a
                // real `Method.invoke` and defer it for later `@ConditionalOnClass`
                // handling) never sees it — it gets a plain array back, and a
                // later `Class[]`→`String[]` conversion NPEs on the missing
                // element instead.
                //
                // The same collapse now covers ANNOTATION-valued arrays, whose
                // entries can carry the sentinel too when a contained annotation's
                // own type is unresolvable — the `@Repeatable`-container shape that
                // JUnit's `AnnotationUtils.findRepeatableAnnotations` walks
                // (`candidateAnnotationType.equals(containerType)` on every entry).
                // Detecting the sentinel by class rather than by declared component
                // is unambiguous: JLS §9.6.1 admits only primitives, `String`,
                // `Class`, enums, annotations and arrays of those as member values,
                // so a `TypeNotPresentException` in this array is always one of ours.
                // Record only the INDEX of a sentinel hit, not the `Value` itself
                // — a later iteration's allocation could relocate it, so re-read
                // it fresh from the (freshly re-pinned) array after the loop
                // instead of carrying a Rust-local copy across further
                // allocating calls.
                let mut sentinel_index: Option<usize> = None;
                let mut arr = arr;
                for (i, elem) in elems.iter().enumerate() {
                    let loader_cur = match (container_loader, container_loader_pin) {
                        (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                        _ => None,
                    };
                    let v = annotation_element_to_java_typed(
                        ctx,
                        elem,
                        elem_desc.as_deref(),
                        container_class_id,
                        loader_cur,
                        annotation_class_id,
                    )?;
                    arr = ctx.read_native_pin(arr_pin, arr);
                    let value_pin = match v {
                        Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
                        _ => None,
                    };
                    let v_cur = match (v, value_pin) {
                        (Value::Object(Some(object)), Some(pin)) => {
                            Value::Object(Some(ctx.read_native_pin(pin, object)))
                        }
                        _ => v,
                    };
                    if sentinel_index.is_none() {
                        if let Value::Object(Some(o)) = v_cur {
                            if ctx
                                .class_name_arc_of_id(ctx.class_id_of_object(o))
                                .as_deref()
                                == Some("java/lang/TypeNotPresentException")
                            {
                                sentinel_index = Some(i);
                            }
                        }
                    }
                    // Always finish populating `arr` — matches the allocate-then-
                    // immediately-fill invariant every other caller of
                    // `new_ref_array` in this function relies on. An early return
                    // here left `arr` allocated but only partially filled (some
                    // trailing slots never touched by `set_array_element`) and
                    // unreferenced by anything, which triggered heap corruption
                    // (`gen_heap::get_field` "undersized object layout" errors on
                    // unrelated `String` objects) under GC — the array is
                    // discarded below when a sentinel was found, but only AFTER
                    // it's fully built and briefly reachable in the normal way.
                    ctx.set_array_element(arr, i, v_cur);
                    if let Some(pin) = value_pin {
                        ctx.unpin_native_roots(pin);
                    }
                }
                arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                if let Some(idx) = sentinel_index {
                    return Ok(ctx.get_array_element(arr, idx));
                }
                Ok(Value::Object(Some(arr)))
            }
        }
    })()?;
    if let Some(pin) = container_loader_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(result)
}

/// Build an Annotation[] array from annotation data.
/// Resolve the `ClassId` of `java/lang/annotation/Annotation` so annotation
/// arrays are allocated with the correct component type. Returns `ClassId(0)`
/// (Object component) only if the class somehow can't be resolved.
///
/// Without this, `getDeclaredAnnotations()` / `getParameterAnnotations()` etc.
/// return `Object[]` / `Object[][]` instead of `Annotation[]` / `Annotation[][]`,
/// so a downstream `(Annotation[][]) result` cast (e.g. ByteBuddy's
/// `JavaDispatcher`-backed reflective `Executable.getParameterAnnotations()`
/// used by Hibernate's `BytecodeProviderImpl`) throws a `ClassCastException`.
thread_local! {
    /// Bootstrap `Annotation` class id, scoped to both the owning VM and the
    /// mutator thread. This avoids a class-manager lookup on every reflective
    /// annotation-array allocation without leaking a VM-local ClassId into a
    /// subsequent in-process VM.
    pub static ANNOTATION_COMPONENT_CLASS_CACHE: std::cell::Cell<Option<(usize, ClassId)>> =
        const { std::cell::Cell::new(None) };
}

pub fn annotation_component_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    // Reflection-heavy frameworks ask for annotation arrays thousands of times
    // per test class. `class_id_by_name` takes the class-manager lock, so cache
    // the bootstrap `Annotation` id per VM and mutator thread. ClassId values
    // are VM-local; keying by `vm_identity` is essential for in-process VM
    // tests.
    let scope = ctx.vm_identity();
    if let Some(class_id) = ANNOTATION_COMPONENT_CLASS_CACHE.with(|cache| {
        cache
            .get()
            .and_then(|(cached_scope, class_id)| (cached_scope == scope).then_some(class_id))
    }) {
        return class_id;
    }

    let class_id = ctx
        .class_id_by_name("java/lang/annotation/Annotation")
        .or_else(|| {
            ctx.ensure_class_initialized("java/lang/annotation/Annotation")
                .ok()
        })
        .unwrap_or(ClassId::new(0));
    // Do not cache the synthetic fallback: a later call may be able to load
    // Annotation after bootstrap has progressed.
    if class_id.as_u32() != 0 {
        ANNOTATION_COMPONENT_CLASS_CACHE.with(|cache| cache.set(Some((scope, class_id))));
    }
    class_id
}

/// Resolve the `ClassId` of `Annotation[]` (`[Ljava/lang/annotation/Annotation;`)
/// вЂ” the component type of an `Annotation[][]` (the `getParameterAnnotations()`
/// return type). See [`annotation_component_class_id`].
///
/// Array class names aren't always pre-loaded by `class_id_by_name`, so as a
/// robust fallback we derive the id from a freshly-allocated `Annotation[0]`
/// (whose runtime class IS `[Ljava/lang/annotation/Annotation;`).
pub fn annotation_array_component_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    if let Some(cid) = ctx.class_id_by_name("[Ljava/lang/annotation/Annotation;") {
        return cid;
    }
    // SB-02b-#5: resolve the `Annotation[]` array class via its mirror, which
    // registers the array class correctly. The previous derive-via-sample path
    // (`class_id_of_object(new_ref_array(Annotation, 0))`) mis-resolved the
    // array class on COLD use, returning the `Annotation` component id instead
    // of `[Annotation` вЂ” so the FIRST `getParameterAnnotations()` call built a
    // 1-D `Annotation[]` outer array instead of `Annotation[][]`. kotlin-reflect's
    // `ReflectClassStructure.loadMethodAnnotations` then `checkcast`s the result
    // to `[[Annotation` and throws ClassCastException (only on the first method
    // processed; later calls hit the now-cached class and were already fine).
    let mirror = descriptor_to_class_mirror(ctx, "[Ljava/lang/annotation/Annotation;");
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        return cid;
    }
    let comp = annotation_component_class_id(ctx);
    let sample = ctx.new_ref_array(comp, 0);
    ctx.class_id_of_object(sample)
}

/// Resolve one annotation's declared TYPE, as seen from `declaring_class_id`.
///
/// Was `annotation_type_loadable_near`, a `bool`. Returning the `ClassId` is
/// the point: the answer is now handed to the builder instead of being
/// recomputed there — see [`resolvable_annotations`].
pub fn resolve_annotation_type_near(
    ctx: &mut dyn NativeContext,
    ann: &cratonvm_native_api::AnnotationData,
    declaring_class_id: Option<ClassId>,
) -> Option<ClassId> {
    let name = annotation_desc_to_class_name(&ann.type_descriptor)?.to_string();
    resolve_annotation_type_class_id(ctx, &name, declaring_class_id)
}

/// Partition `annotations` into the ones whose declared type resolves, pairing
/// each with the resolved `ClassId`.
///
/// The JDK's `sun.reflect.annotation.AnnotationParser` OMITS any annotation
/// whose type class is not resolvable (a compile-only annotation like
/// `org.apiguardian.api.API`, whose jar isn't on the runtime classpath) — it
/// does NOT surface a broken/null annotation, and neither may we.
///
/// Threading the resolved `ClassId` through to
/// [`create_annotation_proxy_with_type`] — rather than letting the builder
/// re-derive it — is what makes that guarantee structural: the filter and the
/// builder cannot disagree, because there is only one lookup.
pub fn resolvable_annotations<'a>(
    ctx: &mut dyn NativeContext,
    declaring_class_id: Option<ClassId>,
    annotations: &'a [cratonvm_native_api::AnnotationData],
) -> Vec<(&'a cratonvm_native_api::AnnotationData, ClassId)> {
    annotations
        .iter()
        .filter_map(|a| {
            let cid = resolve_annotation_type_near(ctx, a, declaring_class_id)?;
            Some((a, cid))
        })
        .collect()
}

pub fn build_annotation_array(
    ctx: &mut dyn NativeContext,
    annotations: &[cratonvm_native_api::AnnotationData],
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(build_annotation_array_for(ctx, None, annotations)?)
}

/// Like [`build_annotation_array`] but resolves each annotation TYPE through
/// the declaring class's defining loader (HotSpot `AnnotationParser`'s
/// "container"). Under classloader isolation (Spring's
/// `@CompileWithForkedClassLoader` fork re-defines the whole framework), the
/// member's declaring class and the framework code comparing annotation types
/// live in the SAME loader; resolving the annotation type globally instead
/// yields the app loader's copy and every `annotationType()` identity
/// comparison silently fails (e.g. `@Autowired` detection returning no
/// injection metadata).
pub fn build_annotation_array_for(
    ctx: &mut dyn NativeContext,
    declaring_class_id: Option<ClassId>,
    annotations: &[cratonvm_native_api::AnnotationData],
) -> Result<ObjectRef, MethodCallFailed> {
    let comp = annotation_component_class_id(ctx);
    let resolvable = resolvable_annotations(ctx, declaring_class_id, annotations);
    let container_loader = declaring_class_id.and_then(|cid| annotation_container_loader(ctx, cid));
    let container_loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    // GC-safe: `create_annotation_proxy_with_type` allocates (see `build_mirror_array`).
    let array = build_mirror_array_comp(ctx, comp, resolvable.len(), |ctx, i| {
        let loader_cur = match (container_loader, container_loader_pin) {
            (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
            _ => None,
        };
        let (ann, ann_cid) = resolvable[i];
        Ok(create_annotation_proxy_with_type(
            ctx,
            ann,
            ann_cid,
            declaring_class_id,
            loader_cur,
        )?)
    })?;
    if let Some(pin) = container_loader_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(array)
}

/// Like [`build_annotation_array`] but routes each proxy through the per-class
/// identity cache, so `getDeclaredAnnotations()` / `getAnnotations()` return the
/// SAME instances `getAnnotation()` returns for `queried_class_id`.
pub fn build_class_annotation_array(
    ctx: &mut dyn NativeContext,
    queried_class_id: ClassId,
    annotations: &[cratonvm_native_api::AnnotationData],
) -> Result<ObjectRef, MethodCallFailed> {
    // Omit annotations whose type isn't loadable вЂ” see `resolvable_annotations`.
    let resolvable = resolvable_annotations(ctx, Some(queried_class_id), annotations);
    // Component is `java/lang/annotation/Annotation`, exactly as
    // `build_method_annotation_array` and `build_annotation_array_for` already
    // use — NOT `ClassId::new(0)`. A reference array carries its component class
    // id in `ObjectHeader.class_id`, so `ClassId::new(0)` made every
    // `Class.getDeclaredAnnotations()` / `Class.getAnnotations()` result report
    // `[Ljava.lang.Object;` where HotSpot reports
    // `[Ljava.lang.annotation.Annotation;` — and, because that same header word
    // is what a virtual/interface dispatch reads to name the receiver's class,
    // an `annotationType()` reached through the array surfaced as
    // `NoSuchMethodError: java.lang.Object.annotationType()`.
    let comp = annotation_component_class_id(ctx);
    // GC-safe: `cached_annotation_proxy` allocates (see `build_mirror_array`).
    Ok(build_mirror_array_comp(
        ctx,
        comp,
        resolvable.len(),
        |ctx, i| {
            let (ann, ann_cid) = resolvable[i];
            Ok(cached_annotation_proxy(
                ctx,
                queried_class_id,
                ann,
                ann_cid,
            )?)
        },
    )?)
}

/// Equivalent to [`build_class_annotation_array`] for Method and Constructor.
/// The returned array is always fresh, as required by the reflection API, while
/// its annotation elements retain the identity HotSpot caches per executable.
pub fn build_method_annotation_array(
    ctx: &mut dyn NativeContext,
    declaring_class_id: ClassId,
    method_name: &str,
    method_desc: &str,
    annotations: &[cratonvm_native_api::AnnotationData],
) -> Result<ObjectRef, MethodCallFailed> {
    let resolvable = resolvable_annotations(ctx, Some(declaring_class_id), annotations);
    let component = annotation_component_class_id(ctx);
    Ok(build_mirror_array_comp(
        ctx,
        component,
        resolvable.len(),
        |ctx, i| {
            let (ann, ann_cid) = resolvable[i];
            Ok(cached_method_annotation_proxy(
                ctx,
                declaring_class_id,
                method_name,
                method_desc,
                ann,
                ann_cid,
            )?)
        },
    )?)
}

/// Class.getDeclaredAnnotations() вЂ” only this class's own annotations.
pub fn native_class_get_declared_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // Even the degenerate result is an `Annotation[]`, not an `Object[]`.
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let annotations = ctx.class_annotations(class_id);
    if crate::nbflags().ann_trace {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        if cn.contains("SpringBootApplication")
            || cn.contains("EnableAutoConfiguration")
            || cn.contains("SpringBootConfiguration")
        {
            eprintln!("[GDA] {} -> {} annotations", cn, annotations.len());
            for a in &annotations {
                eprintln!("    {}", a.type_descriptor);
            }
        }
    }
    let arr = build_class_annotation_array(ctx, class_id, &annotations);
    Ok(Some(Value::Object(Some(arr?))))
}

/// Class.getAnnotations() вЂ” includes @Inherited annotations from superclasses.
pub fn native_class_get_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // Even the degenerate result is an `Annotation[]`, not an `Object[]`.
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };

    let mut annotations = ctx.class_annotations(class_id);

    // Collect type descriptors already present on this class
    let mut seen: std::collections::HashSet<String> = annotations
        .iter()
        .map(|a| a.type_descriptor.clone())
        .collect();

    // Walk superclass chain for @Inherited annotations
    let mut current = ctx.superclass_of(class_id);
    while let Some(super_id) = current {
        let super_anns = ctx.class_annotations(super_id);
        for ann in super_anns {
            if seen.contains(&ann.type_descriptor) {
                continue;
            }
            if is_inherited_annotation(ctx, &ann.type_descriptor) {
                seen.insert(ann.type_descriptor.clone());
                annotations.push(ann);
            }
        }
        current = ctx.superclass_of(super_id);
    }

    let arr = build_class_annotation_array(ctx, class_id, &annotations);
    Ok(Some(Value::Object(Some(arr?))))
}

/// Check if an annotation type is marked with @Inherited.
pub fn is_inherited_annotation(ctx: &mut dyn NativeContext, ann_type_desc: &str) -> bool {
    let class_name = match annotation_desc_to_class_name(ann_type_desc) {
        Some(n) => n,
        None => return false,
    };
    // Try to find already-loaded class first; if not loaded, load it.
    let ann_class_id = match ctx.class_id_by_name(class_name) {
        Some(id) => id,
        None => {
            // Attempt to load the annotation type class
            match ctx.ensure_class_initialized(class_name) {
                Ok(id) => id,
                Err(_) => return false,
            }
        }
    };
    let meta_annotations = ctx.class_annotations(ann_class_id);
    meta_annotations
        .iter()
        .any(|a| a.type_descriptor == "Ljava/lang/annotation/Inherited;")
}

/// Class.getDeclaredAnnotation(Class) вЂ” only this class's own annotations.
pub fn native_class_get_declared_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => return Err(annotation_class_null(ctx, this, AnnotationLookup::Direct)),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_class_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    let target_desc = format!("L{};", ann_class_name);
    let annotations = ctx.class_annotations(class_id);
    for ann in &annotations {
        if ann.type_descriptor == target_desc {
            // W7-26 — `?`, not `if let Ok(..)`. This was the first of five
            // single-annotation sites that dropped the builder's `Err` and fell
            // through to the `Ok(Some(Value::Object(None)))` below, i.e. to
            // **null**. Null is a LEGITIMATE answer here — it means "not
            // present" — so the caller has no way to tell a VM failure from an
            // absent annotation, which is why the refusal measured in W7-12
            // surfaced three frames away as a bare `AssertionError` instead of
            // naming its class. Its array-valued siblings
            // (`build_class_annotation_array` and friends) have always used
            // `?`, and so does `native_method_get_annotation` a few hundred
            // lines down; one run of the pre-fix binary showed both faces of the
            // same cause at once — `RJdkJmx` (array path) got
            // `NoClassDefFoundError: java/lang/annotation/AnnotationProxy`
            // verbatim while `RReflect` (this path) got null.
            //
            // The discrimination line is the one W7-20 drew for the collection
            // helpers: **`Err` propagates; `Ok`-with-nothing-usable stays
            // empty.** `cached_annotation_proxy_resolving` still answers
            // `Ok(None)` for an annotation type it cannot resolve, and that
            // keeps falling through to null exactly as before — only the `Err`
            // arm moves. `MethodCallFailed::ExceptionThrown` is the arm that
            // matters most: it means a real exception is already pending in the
            // VM, and the old spelling both hid it and kept calling back into
            // the VM underneath it.
            if let Some(proxy) = cached_annotation_proxy_resolving(ctx, class_id, ann)? {
                return Ok(Some(Value::Object(Some(proxy))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Class.getAnnotation(Class) вЂ” searches superclass chain for @Inherited.
pub fn native_class_get_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => return Err(annotation_class_null(ctx, this, AnnotationLookup::Direct)),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_class_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    let target_desc = format!("L{};", ann_class_name);

    // Check this class first
    let annotations = ctx.class_annotations(class_id);
    for ann in &annotations {
        if ann.type_descriptor == target_desc {
            // W7-26 — `?`. See `native_class_get_declared_annotation` above for
            // the full argument; this is the site the `RReflect` and
            // `RJdkReflect` assertions actually ran through.
            if let Some(proxy) = cached_annotation_proxy_resolving(ctx, class_id, ann)? {
                return Ok(Some(Value::Object(Some(proxy))));
            }
        }
    }

    // Walk superclass chain for @Inherited annotations
    if is_inherited_annotation(ctx, &target_desc) {
        let mut current = ctx.superclass_of(class_id);
        while let Some(super_id) = current {
            let super_anns = ctx.class_annotations(super_id);
            for ann in &super_anns {
                if ann.type_descriptor == target_desc {
                    // Key by the queried class (class_id), matching HotSpot's
                    // per-class annotationData for inherited annotations.
                    //
                    // W7-26 — `?`. Propagating matters more here than at the
                    // two flat sites, not less: this is a LOOP, so the old
                    // spelling walked on to the next superclass with an
                    // exception already pending in the VM and kept calling
                    // `class_annotations` / the builder underneath it. HotSpot
                    // would have bailed at the first `CHECK`.
                    if let Some(proxy) = cached_annotation_proxy_resolving(ctx, class_id, ann)? {
                        return Ok(Some(Value::Object(Some(proxy))));
                    }
                }
            }
            current = ctx.superclass_of(super_id);
        }
    }

    Ok(Some(Value::Object(None)))
}

