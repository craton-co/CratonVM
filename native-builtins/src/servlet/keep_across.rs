#[allow(unused_imports)]
use super::*;


/// Run `op`, which can collect (it allocates or runs Java), with every object
/// in `vals` pinned, then rewrite `vals` with their current addresses.
///
/// gc-common w13-d: a raw `ObjectRef` a native holds in a Rust local is stale
/// once a moving collection runs, and under the Generational non-moving young
/// sweep an unrooted one is reclaimed in place. Several natives in this file
/// kept an argument or a fresh object across such a call and then stored,
/// returned or dispatched on the old address. This is the one pin/re-read
/// shape they now share; non-object values pass through untouched.
pub fn keep_across<T>(
    ctx: &mut dyn NativeContext,
    vals: &mut [Value],
    op: impl FnOnce(&mut dyn NativeContext) -> T,
) -> T {
    let mut pins: Vec<Option<usize>> = Vec::with_capacity(vals.len());
    for v in vals.iter() {
        pins.push(match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        });
    }
    let out = op(&mut *ctx);
    for (v, pin) in vals.iter_mut().zip(pins.iter()) {
        if let (Some(handle), Value::Object(Some(o))) = (*pin, *v) {
            *v = Value::Object(Some(ctx.read_native_pin(handle, o)));
        }
    }
    if let Some(base) = pins.iter().flatten().next() {
        ctx.unpin_native_roots(*base);
    }
    out
}

/// [`keep_across`] for one object.
pub fn keep_one_across<T>(
    ctx: &mut dyn NativeContext,
    obj: &mut ObjectRef,
    op: impl FnOnce(&mut dyn NativeContext) -> T,
) -> T {
    let mut vals = [Value::Object(Some(*obj))];
    let out = keep_across(ctx, &mut vals, op);
    if let Value::Object(Some(o)) = vals[0] {
        *obj = o;
    }
    out
}

#[cfg(feature = "synthetic-jdk")]
pub fn r3_get_input_stream(ctx: &dyn NativeContext, buffered_reader: ObjectRef) -> Option<ObjectRef> {
    // BufferedReader.field[0] = Reader (InputStreamReader)
    let reader = match ctx.get_field(buffered_reader, 0) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    // InputStreamReader.field[0] = InputStream
    match ctx.get_field(reader, 0) {
        Value::Object(Some(is)) => Some(is),
        _ => None,
    }
}

/// Resolve a resource name relative to a Class mirror.
///
/// Per JLS: if `name` starts with `/`, strip it (absolute). Otherwise,
/// prepend the package path of the class (e.g., `com/example/` for
/// class `com/example/Foo`).
pub fn resolve_class_resource_name(
    ctx: &dyn NativeContext,
    class_mirror: ObjectRef,
    name: &str,
) -> String {
    if name.starts_with('/') {
        return name[1..].to_string();
    }
    // Get the class name from the mirror to compute the package path.
    if let Some(class_id) = crate::lang_class::mirror_class_id(ctx, class_mirror) {
        if let Some(class_name) = ctx.class_name_of_id(class_id) {
            // class_name is like "com/example/Foo" → package is "com/example/"
            if let Some(pos) = class_name.rfind('/') {
                let mut resolved = class_name[..=pos].to_string();
                resolved.push_str(name);
                return resolved;
            }
        }
    }
    // No package (default package) — use name as-is.
    name.to_string()
}

pub fn byte_array_to_vec(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        bytes.push(ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8);
    }
    bytes
}

pub fn charset_name_from_object(ctx: &mut dyn NativeContext, charset: ObjectRef) -> Option<String> {
    // The field fallback reads `charset` after `name()` ran Java.
    let mut charset = charset;
    let receiver = charset;
    let named = keep_one_across(ctx, &mut charset, |ctx| {
        ctx.invoke_virtual(receiver, "name", "()Ljava/lang/String;", &[])
    });
    match named {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => match ctx.get_field_by_name(charset, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
}

pub fn spring_mock_response_content_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<u8>, RuntimeError> {
    match ctx.invoke_virtual(this, "getContentAsByteArray", "()[B", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => Ok(byte_array_to_vec(ctx, arr)),
        _ => Ok(Vec::new()),
    }
}

pub fn spring_mock_response_get_content_as_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    // `getCharacterEncoding` runs Java; `this` is dispatched on again below.
    let receiver = this;
    let charset = keep_one_across(ctx, &mut this, |ctx| {
        match ctx.invoke_virtual(
            receiver,
            "getCharacterEncoding",
            "()Ljava/lang/String;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(s)))) => {
                ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string())
            }
            _ => "UTF-8".to_string(),
        }
    });
    let bytes = spring_mock_response_content_bytes(ctx, this)?;
    let text = crate::charset::decode_str_named(&charset, &bytes);
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

pub fn spring_mock_response_get_content_as_string_charset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    // `Charset.name()` runs Java; `this` is dispatched on again below.
    let charset = match args.get(1) {
        Some(Value::Object(Some(cs))) => {
            let cs = *cs;
            keep_one_across(ctx, &mut this, |ctx| charset_name_from_object(ctx, cs))
                .unwrap_or_else(|| "UTF-8".to_string())
        }
        _ => "UTF-8".to_string(),
    };
    let bytes = spring_mock_response_content_bytes(ctx, this)?;
    let text = crate::charset::decode_str_named(&charset, &bytes);
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

pub fn jython_py_bool(ctx: &mut dyn NativeContext, value: bool) -> MethodCallResult {
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        let field = if value { "True" } else { "False" };
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, field) {
            if let Value::Object(Some(obj)) = ctx.get_static_field(py_cid, idx) {
                return Ok(Some(Value::Object(Some(obj))));
            }
        }
    }
    ctx.new_object_initialized(
        "org/python/core/PyBoolean",
        "(Z)V",
        &[Value::Int(if value { 1 } else { 0 })],
    )
}

pub fn jython_py_none(ctx: &mut dyn NativeContext) -> Value {
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "None") {
            if let Value::Object(Some(obj)) = ctx.get_static_field(py_cid, idx) {
                return Value::Object(Some(obj));
            }
        }
    }
    Value::Object(None)
}

pub fn jython_object_class_is(ctx: &mut dyn NativeContext, obj: ObjectRef, expected: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(expected)
}

pub fn jython_map_field(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    let field = if jython_object_class_is(ctx, obj, "org/python/core/PyDictionary") {
        "internalMap"
    } else {
        "table"
    };
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(map)) => Some(map),
        _ => None,
    }
}

pub fn jython_pystringmap_put_all(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    source: ObjectRef,
) -> bool {
    if !jython_object_class_is(ctx, target, "org/python/core/PyStringMap") {
        return false;
    }
    if !matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(source))
            .as_deref(),
        Some("org/python/core/PyStringMap" | "org/python/core/PyDictionary")
    ) {
        return false;
    }
    let Some(target_map) = jython_map_field(ctx, target) else {
        return false;
    };
    let Some(source_map) = jython_map_field(ctx, source) else {
        return false;
    };
    ctx.invoke_virtual(
        target_map,
        "putAll",
        "(Ljava/util/Map;)V",
        &[Value::Object(Some(source_map))],
    )
    .is_ok()
}

pub fn jython_new_pystringmap(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized("org/python/core/PyStringMap", "()V", &[])? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: "failed to create Jython PyStringMap".to_string(),
        }
        .into()),
    }
}

pub fn jython_new_pyinteger(
    ctx: &mut dyn NativeContext,
    value: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized("org/python/core/PyInteger", "(I)V", &[Value::Int(value)])? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: format!("failed to create Jython PyInteger {value}"),
        }
        .into()),
    }
}

pub fn jython_pyobject_finditem_string(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    key: &str,
) -> Option<ObjectRef> {
    // GC-safety: `create_string` below can trigger a collection that
    // relocates `target` (the receiver of the following `invoke_virtual`);
    // pin it and re-read the forwarded reference before that call.
    let target_pin = ctx.pin_native_root(target);
    let key_obj = ctx.create_string(key);
    let target = ctx.read_native_pin(target_pin, target);
    ctx.unpin_native_roots(target_pin);
    match ctx.invoke_virtual(
        target,
        "__finditem__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        &[Value::Object(Some(key_obj))],
    ) {
        Ok(Some(Value::Object(Some(obj)))) => Some(obj),
        _ => None,
    }
}

pub fn jython_pyobject_setitem_string(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    key: &str,
    value: Value,
) -> Result<(), MethodCallFailed> {
    // GC-safety: `create_string` below can trigger a collection that
    // relocates `target` and/or the object (if any) wrapped by `value`;
    // pin both and re-read the forwarded references before `invoke_virtual`.
    let target_pin = ctx.pin_native_root(target);
    let value_pin = match value {
        Value::Object(Some(v)) => Some((ctx.pin_native_root(v), v)),
        _ => None,
    };
    let key_obj = ctx.create_string(key);
    let target = ctx.read_native_pin(target_pin, target);
    let value = match value_pin {
        Some((pin, v)) => Value::Object(Some(ctx.read_native_pin(pin, v))),
        None => value,
    };
    ctx.unpin_native_roots(target_pin);
    let _ = ctx.invoke_virtual(
        target,
        "__setitem__",
        "(Ljava/lang/String;Lorg/python/core/PyObject;)V",
        &[Value::Object(Some(key_obj)), value],
    )?;
    Ok(())
}

pub fn jython_module_dict(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Value::Object(Some(dict)) = ctx.get_field_by_name(module, "__dict__") {
        return Ok(dict);
    }
    // GC-safety: `jython_new_pystringmap` below allocates and can trigger a
    // collection that relocates `module`; pin it and re-read the forwarded
    // reference before the `set_field_by_name` that uses it as receiver.
    let module_pin = ctx.pin_native_root(module);
    let dict = jython_new_pystringmap(ctx)?;
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(module_pin);
    ctx.set_field_by_name(module, "__dict__", Value::Object(Some(dict)));
    Ok(dict)
}

pub fn jython_new_module(
    ctx: &mut dyn NativeContext,
    name: &str,
    dict: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC: a reference held in a Rust local across an allocating or Java-re-entering
    // call goes stale under a moving collector, and under the Generational
    // non-moving young sweep an unrooted object is ZEROED in place. Pin and
    // re-read. `safe_native_call_impl` truncates `native_pin_roots` when the native
    // returns, so an unmatched pin costs nothing on an error path. See
    // `internal/audits/wide-tranche-triage-20260907.md`.
    // `create_string` allocates, so `dict` — held since entry — is a pre-call
    // address by the time it is handed to the constructor.
    let dict_pin = match dict {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let name_obj = ctx.create_string(name);
    let dict = match dict_pin {
        Some((p, o)) => Value::Object(Some(ctx.read_native_pin(p, o))),
        None => dict,
    };
    let module = match ctx.new_object_initialized(
        "org/python/core/PyModule",
        "(Ljava/lang/String;Lorg/python/core/PyObject;)V",
        &[Value::Object(Some(name_obj)), dict],
    )? {
        Some(Value::Object(Some(obj))) => obj,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("failed to create Jython module {name}"),
            }
            .into())
        }
    };
    // GC-safety: `jython_module_dict` below can itself allocate (a fresh
    // module has no `__dict__` yet); pin `module` and re-read before
    // returning it.
    let module_pin = ctx.pin_native_root(module);
    let _ = jython_module_dict(ctx, module)?;
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(module_pin);
    Ok(module)
}

pub fn jython_ensure_sre_module_attrs(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let dict = jython_module_dict(ctx, module)?;
    // gc-common w13-d: both calls below allocate and run Java, and `dict` is
    // used on every pass; it was the raw first-pass address on the second and
    // third. (An error return leaves the pin to the native-call epilogue.)
    let dict_pin = ctx.pin_native_root(dict);
    for (name, value) in [("MAGIC", 20031017), ("MAXREPEAT", 65535), ("CODESIZE", 4)] {
        let integer = jython_new_pyinteger(ctx, value)?;
        let dict = ctx.read_native_pin(dict_pin, dict);
        jython_pyobject_setitem_string(ctx, dict, name, Value::Object(Some(integer)))?;
    }
    ctx.unpin_native_roots(dict_pin);
    Ok(())
}

pub fn jython_pyobject_text(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    if let Some(text) = jython_py_string_value(ctx, obj) {
        return Some(text);
    }
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

pub fn jython_pymodule_name(ctx: &mut dyn NativeContext, module: ObjectRef) -> Option<String> {
    let dict = jython_module_dict(ctx, module).ok()?;
    let name = jython_pyobject_finditem_string(ctx, dict, "__name__")?;
    jython_pyobject_text(ctx, name)
}

pub fn jython_pymodule_package_lookup(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
    attr: &str,
    module_name: &str,
) -> MethodCallResult {
    if attr.is_empty() || module_name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let full_name = format!("{module_name}.{attr}");
    let package_manager = ctx
        .class_id_by_name("org/python/core/PySystemState")
        .and_then(|cid| {
            ctx.static_field_index_by_name(cid, "packageManager")
                .map(|idx| (cid, idx))
        })
        .and_then(|(cid, idx)| match ctx.get_static_field(cid, idx) {
            Value::Object(Some(obj)) => Some(obj),
            _ => None,
        });
    let Some(package_manager) = package_manager else {
        return Ok(Some(Value::Object(None)));
    };
    // GC-safety: `package_manager` is the receiver of the `invoke_virtual`
    // right below `create_string`, and `module` is only consumed much later
    // (past several more GC-triggering calls); pin both up front. `found`
    // is re-pinned each time it's (re)bound and re-read right before its
    // final uses. Unpin once at the end via the earliest handle.
    let package_manager_pin = ctx.pin_native_root(package_manager);
    let module_pin = ctx.pin_native_root(module);
    let full_name_obj = ctx.create_string(&full_name);
    let package_manager = ctx.read_native_pin(package_manager_pin, package_manager);
    let mut found = match ctx.invoke_virtual(
        package_manager,
        "lookupName",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        &[Value::Object(Some(full_name_obj))],
    )? {
        Some(Value::Object(Some(obj))) => obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut found_pin = ctx.pin_native_root(found);

    if let Some(Value::Object(Some(state))) = jython_py_get_or_create_system_state(ctx)? {
        if let Ok(modules) = jython_system_modules(ctx, state) {
            if let Some(existing) = jython_pyobject_finditem_string(ctx, modules, &full_name) {
                found = existing;
                found_pin = ctx.pin_native_root(found);
            }
        }
    }

    let module = ctx.read_native_pin(module_pin, module);
    let dict = jython_module_dict(ctx, module)?;
    let found = ctx.read_native_pin(found_pin, found);
    jython_pyobject_setitem_string(ctx, dict, attr, Value::Object(Some(found)))?;
    // gc-common w13-d: `__setitem__` ran Java; return the current address.
    let found = ctx.read_native_pin(found_pin, found);
    ctx.unpin_native_roots(package_manager_pin);
    Ok(Some(Value::Object(Some(found))))
}

pub fn jython_pymodule_findattr_ex(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let module = obj_arg(args, 0)?;
    let attr_obj = obj_arg(args, 1)?;
    let attr = ctx.read_string(attr_obj).unwrap_or_default();

    // GC-safety: `module` is passed into several more GC-triggering helper
    // calls below (`jython_pymodule_name`, `jython_ensure_sre_module_attrs`,
    // `jython_module_dict`, `jython_pymodule_package_lookup`); pin it up
    // front and re-read the forwarded reference before each use.
    let module_pin = ctx.pin_native_root(module);

    match ctx.invoke_special(
        "org/python/core/PyObject",
        "__findattr_ex__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        args,
    )? {
        Some(Value::Object(Some(found))) => return Ok(Some(Value::Object(Some(found)))),
        Some(Value::Object(None)) | None => {}
        value => return Ok(value),
    }

    let module = ctx.read_native_pin(module_pin, module);
    let module_name = jython_pymodule_name(ctx, module);
    if module_name.as_deref() == Some("_sre")
        && matches!(attr.as_str(), "MAGIC" | "MAXREPEAT" | "CODESIZE")
    {
        let module = ctx.read_native_pin(module_pin, module);
        jython_ensure_sre_module_attrs(ctx, module)?;
        let module = ctx.read_native_pin(module_pin, module);
        let dict = jython_module_dict(ctx, module)?;
        if let Some(found) = jython_pyobject_finditem_string(ctx, dict, &attr) {
            ctx.unpin_native_roots(module_pin);
            return Ok(Some(Value::Object(Some(found))));
        }
    }

    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(module_pin);
    match module_name {
        Some(name) => jython_pymodule_package_lookup(ctx, module, &attr, &name),
        None => Ok(Some(Value::Object(None))),
    }
}

pub fn jython_pymodule_findattr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jython_pymodule_findattr_ex(ctx, args)
}

pub fn jython_system_modules(
    ctx: &mut dyn NativeContext,
    state: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Value::Object(Some(modules)) = ctx.get_field_by_name(state, "modules") {
        return Ok(modules);
    }
    // gc-common w13-d: the PyStringMap constructor allocates and runs Java;
    // the store must go to `state`'s current address.
    let mut state = state;
    let modules = keep_one_across(ctx, &mut state, |ctx| jython_new_pystringmap(ctx))?;
    ctx.set_field_by_name(state, "modules", Value::Object(Some(modules)));
    Ok(modules)
}

pub fn jython_ensure_builtin_module(
    ctx: &mut dyn NativeContext,
    modules: ObjectRef,
    state: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w13-d: `__finditem__` runs Java, and both `modules` and
    // `state` are read after it.
    let mut kept = [Value::Object(Some(modules)), Value::Object(Some(state))];
    let found = keep_across(ctx, &mut kept, |ctx| {
        jython_pyobject_finditem_string(ctx, modules, "__builtin__")
    });
    let (Value::Object(Some(modules)), Value::Object(Some(state))) = (kept[0], kept[1]) else {
        unreachable!("keep_across keeps an object an object");
    };
    if let Some(module) = found {
        if jython_object_class_is(ctx, module, "org/python/core/PyModule") {
            // GC-safety: `jython_module_dict` can allocate; pin `module`
            // and re-read before returning it.
            let module_pin = ctx.pin_native_root(module);
            let _ = jython_module_dict(ctx, module)?;
            let module = ctx.read_native_pin(module_pin, module);
            ctx.unpin_native_roots(module_pin);
            return Ok(module);
        }
    }

    // GC-safety: `modules` is only used again after `jython_new_pystringmap`/
    // `jython_new_module` (both allocate); pin it across them and re-read
    // before the final `jython_pyobject_setitem_string` call.
    let modules_pin = ctx.pin_native_root(modules);
    let dict = match ctx.get_field_by_name(state, "builtins") {
        Value::Object(Some(obj)) => Value::Object(Some(obj)),
        _ => Value::Object(Some(jython_new_pystringmap(ctx)?)),
    };
    let module = jython_new_module(ctx, "__builtin__", dict)?;
    let module_pin = ctx.pin_native_root(module);
    let modules = ctx.read_native_pin(modules_pin, modules);
    jython_pyobject_setitem_string(ctx, modules, "__builtin__", Value::Object(Some(module)))?;
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(modules_pin);
    Ok(module)
}

pub fn jython_imp_add_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_obj = obj_arg(args, 0)?;
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let state = match jython_py_get_or_create_system_state(ctx)? {
        Some(Value::Object(Some(state))) => state,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Jython PySystemState is unavailable".to_string(),
            }
            .into())
        }
    };
    // GC-safety: `state`/`modules`/`module`/`module_dict` are all read again
    // after later GC-triggering calls in this function (module lookup,
    // dict/builtin-module construction, dict population); pin each right
    // after it's bound and re-read before its next use. Unpin once at the
    // end via the earliest handle (`state_pin`).
    let state_pin = ctx.pin_native_root(state);
    let modules = jython_system_modules(ctx, state)?;
    let modules_pin = ctx.pin_native_root(modules);

    if let Some(module) = jython_pyobject_finditem_string(ctx, modules, &name) {
        if jython_object_class_is(ctx, module, "org/python/core/PyModule") {
            let module_pin = ctx.pin_native_root(module);
            let _ = jython_module_dict(ctx, module)?;
            let mut module = ctx.read_native_pin(module_pin, module);
            if name == "_sre" {
                jython_ensure_sre_module_attrs(ctx, module)?;
                module = ctx.read_native_pin(module_pin, module);
            }
            ctx.unpin_native_roots(state_pin);
            return Ok(Some(Value::Object(Some(module))));
        }
    }

    let module = jython_new_module(ctx, &name, Value::Object(None))?;
    let module_pin = ctx.pin_native_root(module);
    let mut module = module;
    if name == "_sre" {
        jython_ensure_sre_module_attrs(ctx, module)?;
        module = ctx.read_native_pin(module_pin, module);
    }
    let module_dict = jython_module_dict(ctx, module)?;
    let module_dict_pin = ctx.pin_native_root(module_dict);
    let modules = ctx.read_native_pin(modules_pin, modules);
    let state = ctx.read_native_pin(state_pin, state);
    let builtins = jython_ensure_builtin_module(ctx, modules, state)?;
    let builtins_dict = jython_module_dict(ctx, builtins)?;
    let module_dict = ctx.read_native_pin(module_dict_pin, module_dict);
    jython_pyobject_setitem_string(
        ctx,
        module_dict,
        "__builtins__",
        Value::Object(Some(builtins_dict)),
    )?;
    let py_none = jython_py_none(ctx);
    let module_dict = ctx.read_native_pin(module_dict_pin, module_dict);
    jython_pyobject_setitem_string(ctx, module_dict, "__package__", py_none)?;
    let modules = ctx.read_native_pin(modules_pin, modules);
    let module = ctx.read_native_pin(module_pin, module);
    jython_pyobject_setitem_string(ctx, modules, &name, Value::Object(Some(module)))?;
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(state_pin);
    Ok(Some(Value::Object(Some(module))))
}

pub fn jython_pyobject_invoke_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let receiver = obj_arg(args, 0)?;
    let name_obj = obj_arg(args, 1)?;
    let arg = args.get(2).copied().unwrap_or(Value::Object(None));
    let name = ctx.read_string(name_obj).unwrap_or_default();

    if name == "update" && jython_object_class_is(ctx, receiver, "org/python/core/PyStringMap") {
        // gc-common w13-d: a `putAll` that fails has still run Java, and the
        // fallback below dispatches on `receiver` and passes `arg`.
        let mut kept = [Value::Object(Some(receiver)), arg];
        if let Value::Object(Some(source)) = arg {
            if keep_across(ctx, &mut kept, |ctx| {
                jython_pystringmap_put_all(ctx, receiver, source)
            }) {
                return Ok(Some(jython_py_none(ctx)));
            }
        }
        let receiver = obj_arg(&kept, 0)?;
        let _ = ctx.invoke_virtual(
            receiver,
            "update",
            "(Lorg/python/core/PyObject;)V",
            &[kept[1]],
        )?;
        return Ok(Some(jython_py_none(ctx)));
    }

    // gc-common w13-d: `__getattr__` runs Java; `arg` is passed on after it.
    let mut kept_arg = [arg];
    let attr = match keep_across(ctx, &mut kept_arg, |ctx| {
        ctx.invoke_virtual(
            receiver,
            "__getattr__",
            "(Ljava/lang/String;)Lorg/python/core/PyObject;",
            &[Value::Object(Some(name_obj))],
        )
    })? {
        Some(Value::Object(Some(attr))) => attr,
        value => return Ok(value),
    };
    ctx.invoke_virtual(
        attr,
        "__call__",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        &kept_arg,
    )
}

pub fn jython_sre_const_value(attr: &str) -> Option<i32> {
    match attr {
        "MAGIC" => Some(20031017),
        "MAXREPEAT" => Some(65535),
        "CODESIZE" => Some(4),
        _ => None,
    }
}

pub fn jython_pytype_name(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    if !jython_object_class_is(ctx, obj, "org/python/core/PyJavaType") {
        return None;
    }
    match ctx.invoke_virtual(obj, "fastGetName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name),
        _ => None,
    }
}

pub fn jython_pyjavatype_findattr_ex(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let receiver = obj_arg(args, 0)?;
    let attr_obj = obj_arg(args, 1)?;
    let attr = ctx.read_string(attr_obj).unwrap_or_default();
    // gc-common w13-d: `fastGetName` runs Java, and the super call below
    // passes the receiver and the name on; it used to pass the raw `args`.
    let mut kept = [Value::Object(Some(receiver)), Value::Object(Some(attr_obj))];
    let type_name = keep_across(ctx, &mut kept, |ctx| jython_pytype_name(ctx, receiver));
    if matches!(
        type_name.as_deref(),
        Some("org.python.modules._sre" | "_sre")
    ) {
        if let Some(value) = jython_sre_const_value(&attr) {
            let integer = jython_new_pyinteger(ctx, value)?;
            return Ok(Some(Value::Object(Some(integer))));
        }
    }
    ctx.invoke_special(
        "org/python/core/PyType",
        "__findattr_ex__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        &kept,
    )
}

pub fn jython_pyobject_is(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    jython_py_bool(ctx, lhs.as_ptr() == rhs.as_ptr())
}

pub fn jython_pyobject_isnot(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    jython_py_bool(ctx, lhs.as_ptr() != rhs.as_ptr())
}

pub fn jython_py_string_value(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<String> {
    match ctx.get_field_by_name(obj, "string") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

pub fn jython_py_integer_value(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<i64> {
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(obj))?;
    if class_name != "org/python/core/PyInteger"
        && class_name != "org/python/core/PyIntegerDerived"
        && class_name != "org/python/core/PyBoolean"
    {
        return None;
    }
    match ctx.get_field_by_name(obj, "value") {
        Value::Int(v) => Some(v as i64),
        _ => None,
    }
}

pub fn jython_pyobject_equal(ctx: &mut dyn NativeContext, lhs: ObjectRef, rhs: ObjectRef) -> bool {
    if lhs.as_ptr() == rhs.as_ptr() {
        return true;
    }
    if let (Some(l), Some(r)) = (
        jython_py_integer_value(ctx, lhs),
        jython_py_integer_value(ctx, rhs),
    ) {
        return l == r;
    }
    if let (Some(l), Some(r)) = (
        jython_py_string_value(ctx, lhs),
        jython_py_string_value(ctx, rhs),
    ) {
        return l == r;
    }
    false
}

pub fn jython_pyobject_eq(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    let equal = jython_pyobject_equal(ctx, lhs, rhs);
    jython_py_bool(ctx, equal)
}

pub fn jython_pyobject_ne(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    let equal = jython_pyobject_equal(ctx, lhs, rhs);
    jython_py_bool(ctx, !equal)
}

pub fn register_jython_pyobject_natives(r: &mut NativeMethodRegistry) {
    let cls = "org/python/core/PyObject";
    r.register(
        cls,
        "invoke",
        "(Ljava/lang/String;Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_invoke_one, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "_is",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_is, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "_isnot",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_isnot, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "_eq",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_eq, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "_ne",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_ne, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

pub fn jython_find_module_getattr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let receiver = obj_arg(args, 0)?;
    let name_obj = obj_arg(args, 1)?;
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let receiver_class = ctx.class_name_of_id(ctx.class_id_of_object(receiver));
    let exposer_class = match (receiver_class.as_deref(), name.as_str()) {
        (Some("org/python/core/PyNullImporter"), "find_module") => {
            "org/python/core/PyNullImporter$NullImporter_find_module_exposer"
        }
        (Some("org/python/modules/zipimport/zipimporter"), "find_module") => {
            "org/python/modules/zipimport/zipimporter$zipimporter_find_module_exposer"
        }
        (Some("org/python/modules/zipimport/zipimporter"), "load_module") => {
            "org/python/modules/zipimport/zipimporter$zipimporter_load_module_exposer"
        }
        _ => "",
    };
    if !exposer_class.is_empty() {
        // GC-safety: `create_string` and `new_object_initialized` below can
        // each trigger a collection that relocates `receiver` (used as an
        // argument to `bind` only after both complete); pin it and re-read
        // the forwarded reference before that call.
        let receiver_pin = ctx.pin_native_root(receiver);
        let method_name = ctx.create_string(&name);
        let exposer = match ctx.new_object_initialized(
            exposer_class,
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(method_name))],
        )? {
            Some(Value::Object(Some(obj))) => obj,
            value => return Ok(value),
        };
        let receiver = ctx.read_native_pin(receiver_pin, receiver);
        ctx.unpin_native_roots(receiver_pin);
        return ctx.invoke_virtual(
            exposer,
            "bind",
            "(Lorg/python/core/PyObject;)Lorg/python/core/PyBuiltinCallable;",
            &[Value::Object(Some(receiver))],
        );
    }

    Ok(Some(jython_py_none(ctx)))
}

pub fn register_jython_imp_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "org/python/core/imp",
        "addModule",
        "(Ljava/lang/String;)Lorg/python/core/PyModule;",
        jython_imp_add_module, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/python/core/PyModule",
        "__findattr_ex__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_pymodule_findattr_ex, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/python/core/PyModule",
        "__findattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_pymodule_findattr, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/python/core/PyJavaType",
        "__findattr_ex__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_pyjavatype_findattr_ex, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/python/core/PyNullImporter",
        "__getattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_find_module_getattr, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/python/modules/zipimport/zipimporter",
        "__getattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_find_module_getattr, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

pub fn jython_current_thread_system_state(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let mapping = ctx
        .class_id_by_name("org/python/core/Py")
        .and_then(|py_cid| {
            ctx.static_field_index_by_name(py_cid, "threadStateMapping")
                .map(|idx| (py_cid, idx))
        })
        .and_then(|(py_cid, idx)| match ctx.get_static_field(py_cid, idx) {
            Value::Object(Some(mapping)) => Some(mapping),
            _ => None,
        })?;
    let thread_state = match ctx.invoke_virtual(
        mapping,
        "getThreadState",
        "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
        &[Value::Object(None)],
    ) {
        Ok(Some(Value::Object(Some(ts)))) => ts,
        _ => return None,
    };
    match ctx.invoke_virtual(
        thread_state,
        "getSystemState",
        "()Lorg/python/core/PySystemState;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(state)))) => Some(state),
        _ => None,
    }
}

pub fn jython_py_get_or_create_system_state(ctx: &mut dyn NativeContext) -> MethodCallResult {
    if let Some(state) = jython_current_thread_system_state(ctx) {
        return Ok(Some(Value::Object(Some(state))));
    }
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "defaultSystemState") {
            if let Value::Object(Some(obj)) = ctx.get_static_field(py_cid, idx) {
                return Ok(Some(Value::Object(Some(obj))));
            }
            let state =
                match ctx.new_object_initialized("org/python/core/PySystemState", "()V", &[])? {
                    Some(Value::Object(Some(obj))) => obj,
                    _ => return Ok(Some(Value::Object(None))),
                };
            ctx.set_static_field(py_cid, idx, Value::Object(Some(state)));
            return Ok(Some(Value::Object(Some(state))));
        }
    }
    ctx.new_object_initialized("org/python/core/PySystemState", "()V", &[])
}

pub fn jython_py_get_system_state(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    jython_py_get_or_create_system_state(ctx)
}

pub fn jython_py_set_system_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // gc-common w13-d: getting (or creating) the old state can run Java; the
    // new state is stored after it, so it is kept current across that call.
    let mut kept = [args.get(0).copied().unwrap_or(Value::Object(None))];
    let old = keep_across(ctx, &mut kept, |ctx| {
        jython_py_get_or_create_system_state(ctx)
    })?;
    let new_state = kept[0];
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "defaultSystemState") {
            ctx.set_static_field(py_cid, idx, new_state);
        }
    }
    Ok(old)
}

pub fn jython_py_get_thread_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut requested_state = args.get(0).copied().unwrap_or(Value::Object(None));
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "threadStateMapping") {
            if let Value::Object(Some(mapping)) = ctx.get_static_field(py_cid, idx) {
                // gc-common w13-d: a failed `getThreadState` has still run
                // Java, and the fallback below passes the requested state on.
                let mut kept = [requested_state];
                let answered = keep_across(ctx, &mut kept, |ctx| {
                    ctx.invoke_virtual(
                        mapping,
                        "getThreadState",
                        "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
                        &[requested_state],
                    )
                });
                requested_state = kept[0];
                if let Ok(Some(state)) = answered {
                    return Ok(Some(state));
                }
            }
        }
    }

    let sys_state = match requested_state {
        Value::Object(None) => {
            jython_py_get_or_create_system_state(ctx)?.unwrap_or(Value::Object(None))
        }
        v => v,
    };
    ctx.new_object_initialized(
        "org/python/core/ThreadState",
        "(Lorg/python/core/PySystemState;)V",
        &[sys_state],
    )
}

pub fn jython_py_get_thread_state_noarg(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    jython_py_get_thread_state(ctx, &[Value::Object(None)])
}

pub fn jython_py_import_site_if_selected(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    if let Some(options_cid) = ctx.class_id_by_name("org/python/core/Options") {
        if let Some(idx) = ctx.static_field_index_by_name(options_cid, "importSite") {
            ctx.set_static_field(options_cid, idx, Value::Int(0));
        }
        if let Some(idx) = ctx.static_field_index_by_name(options_cid, "no_site") {
            ctx.set_static_field(options_cid, idx, Value::Int(1));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn register_jython_thread_state_natives(r: &mut NativeMethodRegistry) {
    let cls = "org/python/core/Py";
    r.register(
        cls,
        "importSiteIfSelected",
        "()Z",
        jython_py_import_site_if_selected, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "getSystemState",
        "()Lorg/python/core/PySystemState;",
        jython_py_get_system_state, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "setSystemState",
        "(Lorg/python/core/PySystemState;)Lorg/python/core/PySystemState;",
        jython_py_set_system_state, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "getThreadState",
        "()Lorg/python/core/ThreadState;",
        jython_py_get_thread_state_noarg, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "getThreadState",
        "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
        jython_py_get_thread_state, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

pub fn register_spring_mock_response_natives(r: &mut NativeMethodRegistry) {
    for cls in [
        "org/springframework/mock/web/MockHttpServletResponse",
        "org/springframework/web/testfixture/servlet/MockHttpServletResponse",
    ] {
        r.register(
            cls,
            "getContentAsString",
            "()Ljava/lang/String;",
            spring_mock_response_get_content_as_string, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "getContentAsString",
            "(Ljava/nio/charset/Charset;)Ljava/lang/String;",
            spring_mock_response_get_content_as_string_charset, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
}

pub fn native_bool(value: Option<Value>) -> bool {
    matches!(value, Some(Value::Int(v)) if v != 0)
}

pub fn script_engine_manager_create_engine(
    ctx: &mut dyn NativeContext,
    manager: ObjectRef,
    factory: ObjectRef,
) -> MethodCallResult {
    // GC-safety: `getScriptEngine` can trigger a collection that relocates
    // `manager` (read again right after); `engine` (the result) is likewise
    // read again after the later `setBindings` call. Pin both.
    let manager_pin = ctx.pin_native_root(manager);
    let engine = match ctx.invoke_virtual(
        factory,
        "getScriptEngine",
        "()Ljavax/script/ScriptEngine;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(engine)))) => engine,
        Ok(_) | Err(_) => return Ok(Some(Value::Object(None))),
    };
    let manager = ctx.read_native_pin(manager_pin, manager);
    let engine_pin = ctx.pin_native_root(engine);

    if let Value::Object(Some(_)) = ctx.get_field_by_name(manager, "globalScope") {
        let bindings = ctx.get_field_by_name(manager, "globalScope");
        let engine = ctx.read_native_pin(engine_pin, engine);
        if ctx
            .invoke_virtual(
                engine,
                "setBindings",
                "(Ljavax/script/Bindings;I)V",
                &[bindings, Value::Int(200)],
            )
            .is_err()
        {
            ctx.unpin_native_roots(manager_pin);
            return Ok(Some(Value::Object(None)));
        }
    }

    let engine = ctx.read_native_pin(engine_pin, engine);
    ctx.unpin_native_roots(manager_pin);
    Ok(Some(Value::Object(Some(engine))))
}

pub fn native_int(value: Option<Value>) -> Option<i32> {
    match value {
        Some(Value::Int(v)) => Some(v),
        _ => None,
    }
}

pub fn java_list_size(ctx: &mut dyn NativeContext, list: ObjectRef) -> Option<i32> {
    native_int(ctx.invoke_virtual(list, "size", "()I", &[]).ok().flatten())
}

pub fn java_list_get(ctx: &mut dyn NativeContext, list: ObjectRef, index: i32) -> Option<ObjectRef> {
    match ctx
        .invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(index)])
        .ok()
        .flatten()
    {
        Some(Value::Object(Some(obj))) => Some(obj),
        _ => None,
    }
}

pub fn java_string_list_contains(ctx: &mut dyn NativeContext, list: ObjectRef, key: ObjectRef) -> bool {
    let key_text = ctx.read_string(key);
    // gc-common w13-d: `size()` and every `get(i)` run Java. `list` was
    // pinned only inside the loop and `key` not at all, so the identity test
    // compared against the entry address and the final `contains` dispatched
    // on raw entry addresses. Both are pinned for the whole function now.
    let list_pin = ctx.pin_native_root(list);
    let key_pin = ctx.pin_native_root(key);
    if let Some(size) = java_list_size(ctx, list) {
        for index in 0..size {
            let list = ctx.read_native_pin(list_pin, list);
            let Some(item) = java_list_get(ctx, list, index) else {
                continue;
            };
            let key = ctx.read_native_pin(key_pin, key);
            if item.as_ptr() == key.as_ptr() {
                ctx.unpin_native_roots(list_pin);
                return true;
            }
            if let Some(expected) = key_text.as_deref() {
                if ctx.read_string(item).as_deref() == Some(expected) {
                    ctx.unpin_native_roots(list_pin);
                    return true;
                }
            }
        }
    }
    let list = ctx.read_native_pin(list_pin, list);
    let key = ctx.read_native_pin(key_pin, key);
    ctx.unpin_native_roots(list_pin);
    native_bool(
        ctx.invoke_virtual(
            list,
            "contains",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(key))],
        )
        .ok()
        .flatten(),
    )
}

pub fn script_engine_manager_factory_matches(
    ctx: &mut dyn NativeContext,
    factory: ObjectRef,
    key: ObjectRef,
    list_method: &str,
) -> bool {
    let list = match ctx.invoke_virtual(factory, list_method, "()Ljava/util/List;", &[]) {
        Ok(Some(Value::Object(Some(list)))) => list,
        _ => return false,
    };
    java_string_list_contains(ctx, list, key)
}

pub fn script_engine_manager_try_factory(
    ctx: &mut dyn NativeContext,
    manager: ObjectRef,
    factory: ObjectRef,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    match script_engine_manager_create_engine(ctx, manager, factory)? {
        Some(Value::Object(Some(engine))) => Ok(Some(engine)),
        _ => Ok(None),
    }
}

pub fn script_engine_manager_get_engine(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    association_field: &str,
    list_method: &str,
) -> MethodCallResult {
    let manager = obj_arg(args, 0)?;
    let key = obj_arg(args, 1)?;

    // GC-safety: `manager`/`key` are both read again after several
    // GC-triggering calls below (map lookup, factory-list retrieval, the
    // per-factory match/try calls); pin both for the whole function.
    // `factories`/`factory` get their own per-scope pins.
    let manager_pin = ctx.pin_native_root(manager);
    let key_pin = ctx.pin_native_root(key);

    if let Value::Object(Some(map)) = ctx.get_field_by_name(manager, association_field) {
        if let Ok(Some(Value::Object(Some(factory)))) = ctx.invoke_virtual(
            map,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key))],
        ) {
            let manager = ctx.read_native_pin(manager_pin, manager);
            if let Some(engine) = script_engine_manager_try_factory(ctx, manager, factory)? {
                ctx.unpin_native_roots(manager_pin);
                return Ok(Some(Value::Object(Some(engine))));
            }
        }
    }

    let manager = ctx.read_native_pin(manager_pin, manager);
    let factories =
        match ctx.invoke_virtual(manager, "getEngineFactories", "()Ljava/util/List;", &[])? {
            Some(Value::Object(Some(factories))) => factories,
            _ => {
                ctx.unpin_native_roots(manager_pin);
                return Ok(Some(Value::Object(None)));
            }
        };
    let factories_pin = ctx.pin_native_root(factories);
    let Some(size) = java_list_size(ctx, factories) else {
        ctx.unpin_native_roots(manager_pin);
        return Ok(Some(Value::Object(None)));
    };
    for index in 0..size {
        let factories = ctx.read_native_pin(factories_pin, factories);
        let Some(factory) = java_list_get(ctx, factories, index) else {
            continue;
        };
        let factory_pin = ctx.pin_native_root(factory);
        let key = ctx.read_native_pin(key_pin, key);
        if script_engine_manager_factory_matches(ctx, factory, key, list_method) {
            let manager = ctx.read_native_pin(manager_pin, manager);
            let factory = ctx.read_native_pin(factory_pin, factory);
            if let Some(engine) = script_engine_manager_try_factory(ctx, manager, factory)? {
                ctx.unpin_native_roots(manager_pin);
                return Ok(Some(Value::Object(Some(engine))));
            }
        }
        ctx.unpin_native_roots(factory_pin);
    }

    ctx.unpin_native_roots(manager_pin);
    Ok(Some(Value::Object(None)))
}

pub fn script_engine_manager_get_engine_by_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    script_engine_manager_get_engine(ctx, args, "nameAssociations", "getNames")
}

pub fn script_engine_manager_get_engine_by_extension(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    script_engine_manager_get_engine(ctx, args, "extensionAssociations", "getExtensions")
}

pub fn script_engine_manager_get_engine_by_mime_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    script_engine_manager_get_engine(ctx, args, "mimeTypeAssociations", "getMimeTypes")
}

pub fn register_script_engine_manager_natives(r: &mut NativeMethodRegistry) {
    let cls = "javax/script/ScriptEngineManager";
    r.register(
        cls,
        "getEngineByName",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        script_engine_manager_get_engine_by_name, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "getEngineByExtension",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        script_engine_manager_get_engine_by_extension, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "getEngineByMimeType",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        script_engine_manager_get_engine_by_mime_type, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

pub fn register_r3_resource_loading(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;

    // -------------------------------------------------------------------------
    // java.lang.Class.getResourceAsStream(String) → InputStream
    // Returns a ByteArrayInputStream backed by the raw resource bytes.
    // -------------------------------------------------------------------------
    r.register(
        "java/lang/Class",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        |ctx, args| {
            let class_mirror = match args.first() {
                Some(Value::Object(Some(m))) => *m,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            let resource_name = resolve_class_resource_name(ctx, class_mirror, &name);

            match ctx.find_resource(&resource_name) {
                None => Ok(Some(Value::Object(None))),
                Some(bytes) => {
                    // Build a ByteArrayInputStream: field 0 = byte[], field 1 = pos, field 2 = count
                    let mut arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
                    for (i, &b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                    }
                    // gc-common w13-d: the stream allocation can collect; the
                    // buffer is stored into it afterwards.
                    let stream = keep_one_across(ctx, &mut arr, |ctx| {
                        try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)
                    })?;
                    ctx.set_field(stream, 0, Value::Object(Some(arr))); // buf
                    ctx.set_field(stream, 1, Value::Int(0)); // pos
                    ctx.set_field(stream, 2, Value::Int(0)); // mark
                    ctx.set_field(stream, 3, Value::Int(bytes.len() as i32)); // count
                    Ok(Some(Value::Object(Some(stream))))
                }
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -------------------------------------------------------------------------
    // java.lang.Class.getResource(String) → URL
    // Resolves name relative to the class's package, then searches classpath.
    // -------------------------------------------------------------------------
    r.register(
        "java/lang/Class",
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        |ctx, args| {
            let class_mirror = match args.first() {
                Some(Value::Object(Some(m))) => *m,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            let resource_name = resolve_class_resource_name(ctx, class_mirror, &name);

            match ctx.find_resource(&resource_name) {
                None => Ok(Some(Value::Object(None))),
                Some(_) => {
                    // SBR wave 2 (lane `netaddr`): under `--real-jdk` the URL's
                    // own JDK bodies run (`toString` is
                    // `handler.toExternalForm(this)`), so hand out the class
                    // path's real URL for the resource, built by the JDK
                    // constructor with its handler. `--compatible` keeps the
                    // `classpath:` carrier below unchanged.
                    if ctx.is_jdk_only() {
                        let spec = ctx
                            .find_all_resource_urls(&resource_name)
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| format!("classpath:{resource_name}"));
                        let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec)?;
                        return Ok(Some(Value::Object(Some(url))));
                    }
                    let mut url = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 6)?;
                    // gc-common w13-d: the string allocation can collect.
                    let full_str = keep_one_across(ctx, &mut url, |ctx| {
                        ctx.create_string(&format!("classpath:{resource_name}"))
                    });
                    ctx.set_field(url, 5, Value::Object(Some(full_str)));
                    crate::jboss_module_loader::url_write_declared_field_defaults(&*ctx, url);
                    Ok(Some(Value::Object(Some(url))))
                }
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -------------------------------------------------------------------------
    // RDR-MIGRATION 2026-06-01: the synthetic Reader-stack natives below
    // (InputStream.read/close, InputStreamReader.<init>/read/close,
    // BufferedReader.<init>/readLine/lines/close, Reader.close) used to be
    // registered UNCONDITIONALLY. They shadowed the REAL JDK Reader bytecode
    // via `native_methods.find(class_name, …)` and broke every real reader:
    // the blanket `BufferedReader.readLine` returned null for real readers
    // (its synthetic byte[]-backed layout never matched a real FileReader /
    // InputStreamReader), so `new BufferedReader(new FileReader(f)).readLine()`
    // returned 0 lines.
    //
    // The whole java.io Reader stack now runs REAL JDK bytecode
    // (FileReader → InputStreamReader → sun.nio.cs.StreamDecoder → the
    // underlying InputStream), exactly like the FileInputStream/FileOutputStream
    // open0/read0 surface. The StreamDecoder native shim
    // (native-io::stream_decoder) drives `in.read([BII)I` virtually, so it
    // works over a real FileInputStream *and* over the synthetic
    // ByteArrayInputStream produced by `getResourceAsStream` above — meaning
    // the r3 resource-loading use case (`new BufferedReader(new
    // InputStreamReader(getResourceAsStream(...)))`) keeps working end-to-end
    // through real bytecode without any synthetic Reader native.
    //
    // These synthetic shadows are therefore retired in real-JDK mode and only
    // kept under `synthetic-jdk` (the no-real-JDK stub build), where there is
    // no real Reader bytecode to defer to.
    //
    // 2026-09-17: the cfg ALONE was the wrong guard, for the reason
    // `NativeMethodRegistry::drops_real_layout_synthetic` spells out and that
    // the `native-io` twin of this block (the `java/io/InputStreamReader`
    // cluster in `native-io/src/lib.rs`) already learned on 2026-08-12 (W7-50):
    // the Cargo feature decides what is COMPILED, the launcher flag decides
    // which CLASS LIBRARY is LOADED. A `--features synthetic-jdk` binary run
    // against a real JDK satisfied the cfg and registered these OVER the real
    // `java.io.InputStreamReader`, where slot 0 is `Reader.lock` (not a parked
    // `InputStream`) and nothing ever creates the real `sd` StreamDecoder. The
    // real `read([CII)I` — which this block does NOT register, so it keeps
    // running real bytecode — then dereferences that null `sd`, and every
    // `InputStreamReader`/`BufferedReader` read in the image dies with
    // `NullPointerException: ... because "this.sd" is null`. That took out the
    // whole hibernate-reactive suite (188/188 classes) through
    // `ServiceLoader`'s own `BufferedReader.readLine`.
    #[cfg(feature = "synthetic-jdk")]
    if !r.drops_real_layout_synthetic() {
        // DELETED wave 4 (2026-07-28): `java/io/InputStream.close()V` (a no-op)
        // and `java/io/InputStream.read()I` (a constant EOF) used to be
        // registered here. Both were DEAD registrations, in both run modes:
        // `native-io::register_io_natives` registers the same two triples
        // UNGATED (`native-io/src/lib.rs`, the "java.io.InputStream (base class
        // fallback)" block) bound to `native_bais_read` / `native_bais_close`,
        // and it runs strictly AFTER this registrar in every VM init path
        // (`vm/src/vm/vm_init.rs`: register_builtins → register_io_natives, and
        // register_essential_natives_with_shims → register_io_natives).
        // Last-registration-wins, so nothing ever reached the constants here.
        // Do not re-add them: they can only shadow the working implementations.

        // -------------------------------------------------------------------------
        // java.io.InputStreamReader.<init>(InputStream)  — store stream at field 0
        // java.io.InputStreamReader.<init>(InputStream, Charset) — same
        // -------------------------------------------------------------------------
        r.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let stream = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.set_field(this, 0, stream);
                Ok(None)
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let stream = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.set_field(this, 0, stream);
                Ok(None)
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/lang/String;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let stream = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.set_field(this, 0, stream);
                Ok(None)
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // `InputStreamReader.close()` is NOT an empty base-class body: the real
        // one closes its StreamDecoder, which closes the wrapped InputStream.
        // The three synthetic `<init>`s above park that stream in slot 0, so
        // propagate the close to it — a no-op here held the underlying stream
        // (and its OS handle) open for the rest of the process. Clearing the
        // slot keeps `close()` idempotent, as the contract requires; it happens
        // BEFORE the nested dispatch because that call runs arbitrary Java and a
        // moving young GC there would relocate `this`, stranding a write made
        // afterwards (native stale-local family).
        //
        // The delegated failure PROPAGATES: `InputStreamReader.close()` is a
        // bare `sd.close()` under `throws IOException`, and `StreamDecoder`'s
        // `implClose()` is a bare `in.close()` / `ch.close()`. No `catch` on
        // that chain. W7-57-close-flush-swallow-sweep.md
        r.register("java/io/InputStreamReader", "close", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
                ctx.set_field(this, 0, Value::Object(None));
                ctx.invoke_virtual(stream, "close", "()V", &[])?;
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        // `read()` has to consume from the stream those three `<init>`s parked
        // in slot 0, exactly as `close()` above propagates to it. The constant
        // `-1` this replaced reported permanent EOF, so in synthetic mode
        // every `new InputStreamReader(in).read()` — and every BufferedReader
        // layered on one — saw an empty stream rather than the resource's
        // bytes.
        //
        // This is a byte-for-char passthrough: exact for US-ASCII and
        // ISO-8859-1, and it splits a multi-byte UTF-8 sequence into separate
        // chars. That is a known approximation of the real StreamDecoder, and
        // still strictly better than claiming EOF. The default (real-JDK)
        // build never reaches this code — see the RDR-MIGRATION note above.
        r.register("java/io/InputStreamReader", "read", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Value::Object(Some(stream)) = ctx.get_field(this, 0) else {
                // No source, or already closed (close() nulls slot 0).
                return Ok(Some(Value::Int(-1)));
            };
            match ctx.invoke_virtual(stream, "read", "()I", &[])? {
                Some(Value::Int(b)) => Ok(Some(Value::Int(b))),
                _ => Ok(Some(Value::Int(-1))),
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub);

        // Real-JDK BufferedReader methods retain their bytecode implementation.
    } // end synthetic Reader-stack block (cfg + !drops_real_layout_synthetic)
}

// =============================================================================
// S1: URLClassLoader + ServiceLoader (real META-INF/services discovery)
//
// URLClassLoader layout (2 fields):
//   field 0: URL[] — the array of URL objects provided to the constructor
//   field 1: parent ClassLoader ref
//
// ServiceLoader layout (2 fields):
//   field 0: java.lang.Class mirror — the service interface
//   field 1: Object[] — lazily loaded service instances (null until first use)
//
// On construction URLClassLoader registers its URLs with the VM classpath so
// subsequent class loading and resource loading searches those JARs/directories.
// ServiceLoader reads META-INF/services/<interface> from the classpath to
// discover and instantiate service providers.
// =============================================================================

/// `URLClassLoader.close()` for the REAL-JDK path.
///
/// The `close()` inside `register_s1_classloading` (and `classloader::ucl_close`)
/// only runs under `register_synthetic_overrides`, and both are written for the
/// SYNTHETIC carrier: they read the URL array out of a fixed slot and write a
/// `closed` flag into another. Neither is safe on a real `java.net.URLClassLoader`,
/// whose slots belong to `ucp`/`acc`/... — so the real-JDK build needs its own,
/// layout-free version.
///
/// This one asks the loader for its URLs through `getURLs()` (real bytecode, real
/// layout) and retracts exactly those roots. Classes already defined stay
/// defined, which is what HotSpot does: `close()` shuts the `URLClassPath` and
/// unloads nothing.
pub fn register_url_classloader_close_bridge(r: &mut NativeMethodRegistry) {
            r.register("java/net/URLClassLoader", "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Retracting the dynamic-classpath roots (below) is only half of it in
        // real-JDK mode: the loader's own `findClass` there is
        // `classloader_real::ucl_real_find_class`, which searches on its own
        // behalf because the real `ucp` CratonVM hands the loader is never
        // populated. Mark the loader so that native refuses too.
        //
        // Into the per-VM closed set every other closed-loader reader consults
        // (gc-common w12-d): keyed by `(vm, identity hash)`, dropped at VM
        // teardown, pruned when the loader dies. The real-mode set this wrote
        // before was one per process, keyed by the bare hash, so VM A's
        // `close()` shut VM B's loader with the same hash, and never pruned.
        //
        // Marked FIRST, while `this` is the caller's current address: the row
        // stores that address for the reconcile, and `getURLs` below runs Java
        // and can move the loader (the mark used to come after it, on the
        // unrefreshed raw copy). `getURLs` does not consult the closed set, and
        // HotSpot's `getURLs` still answers after `close()`.
        crate::classloader::ucl_mark_closed(ctx, this);
        let urls = ctx
            .invoke_virtual(this, "getURLs", "()[Ljava/net/URL;", &[])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None));
        let paths = s1_url_array_to_fs_paths(ctx, urls);
        if !paths.is_empty() {
            ctx.unregister_dynamic_classpath(&paths);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    }

/// Every filesystem path a `URL[]` names, in order.
///
/// Shared by `URLClassLoader`'s constructors (which register the paths) and by
/// `close()` (which retracts them), so the two cannot compute a different set
/// from the same array and leave a root behind.
pub fn s1_url_array_to_fs_paths(ctx: &mut dyn NativeContext, url_arr_val: Value) -> Vec<String> {
    let Value::Object(Some(arr)) = url_arr_val else {
        return Vec::new();
    };
    let len = ctx.array_length(arr);
    let mut paths = Vec::new();
    for i in 0..len {
        let Value::Object(Some(url_obj)) = ctx.get_array_element(arr, i) else {
            continue;
        };
        // URL field 5 = full string (URL_FIELD_FULL)
        let Value::Object(Some(s)) = ctx.get_field(url_obj, 5) else {
            continue;
        };
        let full_str = ctx.read_string(s).unwrap_or_default();
        if let Some(path) = s1_url_to_fs_path(&full_str) {
            paths.push(path);
        }
    }
    paths
}

/// Extract a filesystem path from a Java URL string.
///
/// Handles:
/// - `file:/path/to/foo.jar`  → `/path/to/foo.jar`
/// - `file:/C:/path/to/dir/`  → `C:/path/to/dir/`  (Windows)
/// - `jar:file:/foo.jar!/`    → `/foo.jar`
/// - bare filesystem paths    → returned as-is
pub fn s1_url_to_fs_path(url_str: &str) -> Option<String> {
    let url_str = url_str.trim();
    if let Some(rest) = url_str.strip_prefix("jar:") {
        // jar:file:/path/to/foo.jar!/some/entry → extract the JAR path
        let inner = rest.split('!').next()?;
        return s1_url_to_fs_path(inner);
    }
    if let Some(rest) = url_str.strip_prefix("file:") {
        // file:/path → /path  (Unix)
        // file:/C:/path → C:/path  (Windows — strip one leading /)
        let after_slash = rest.trim_start_matches('/');
        // Windows drive: "C:/..."
        if after_slash.len() >= 2 && after_slash.chars().nth(1) == Some(':') {
            return Some(after_slash.to_string());
        }
        // Unix absolute path
        return Some(format!("/{after_slash}"));
    }
    // Already a filesystem path (e.g. from internal tests)
    if !url_str.contains("://") {
        return Some(url_str.to_string());
    }
    None
}

/// Ensure a ServiceLoader's services have been discovered and instantiated.
/// Returns the Object[] of loaded providers (may be empty), and stores it
/// in ServiceLoader field 1 for subsequent calls.
pub fn s1_service_loader_ensure_loaded(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    use cratonvm_types::ArrayElementType;

    // If already loaded, return cached array
    if let Value::Object(Some(arr)) = ctx.get_field(sl, 1) {
        return arr;
    }

    let provider_names = s1_service_loader_provider_names(ctx, sl);

    // gc-common w13-d: provider class initialisation, each constructor and
    // the array allocation can all collect. `sl` and every instance made so
    // far are pinned across them. The providers used to be held raw in a
    // `Vec<Value>` across the next provider's constructor, and the result
    // array (and the empty one, allocated first) stored through `sl`'s
    // entry address.
    let sl_pin = ctx.pin_native_root(sl);
    let mut instances: Vec<(usize, ObjectRef)> = Vec::new();
    for provider_name in &provider_names {
        let class_name = provider_name.replace('.', "/");
        if ctx.ensure_class_initialized(&class_name).is_ok() {
            if let Ok(Some(Value::Object(Some(obj)))) =
                ctx.new_object_initialized(&class_name, "()V", &[])
            {
                let pin = ctx.pin_native_root(obj);
                instances.push((pin, obj));
            }
        }
    }

    // Store as Object[]
    let arr = ctx.new_array(ArrayElementType::Reference, instances.len());
    for (i, (pin, obj)) in instances.iter().enumerate() {
        let obj = ctx.read_native_pin(*pin, *obj);
        ctx.set_array_element(arr, i, Value::Object(Some(obj)));
    }
    let sl = ctx.read_native_pin(sl_pin, sl);
    ctx.unpin_native_roots(sl_pin);
    ctx.set_field(sl, 1, Value::Object(Some(arr)));
    arr
}

/// The provider class names (dotted) a ServiceLoader's service interface
/// names, from `META-INF/services` and from JPMS `provides`. Empty when the
/// loader has no resolvable interface. Allocates nothing on the Java heap.
pub fn s1_service_loader_provider_names(ctx: &mut dyn NativeContext, sl: ObjectRef) -> Vec<String> {
    // Get the service interface ClassId from the Class mirror (field 0)
    let mirror = match ctx.get_field(sl, 0) {
        Value::Object(Some(m)) => m,
        _ => return Vec::new(),
    };
    // JDK-ONLY-LAYOUT: converted from `get_field(mirror, 0)` to
    // `mirror_class_id`, which asks the authoritative reverse map first and
    // only then the legacy Int-at-slot-0 overlay. Slot 0 of a real
    // `java.lang.Class` is `cachedConstructor`, a reference; this read worked
    // solely because `get_or_create_class_mirror` deliberately parks the
    // ClassId there, and it was one of the readers that made that overlay
    // load-bearing.
    //
    // The `object_num_fields == 0` guard went with it: a field count cannot
    // tell a real mirror from a mock, and `mirror_class_id` answers `None` for
    // both the empty-object and the no-such-mapping cases anyway.
    let class_id = match crate::lang_class::mirror_class_id(ctx, mirror) {
        Some(cid) => cid,
        None => return Vec::new(),
    };
    let iface_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n.replace('/', "."),
        None => return Vec::new(),
    };

    // Collect provider class names from two sources:
    // 1. META-INF/services/<interface-name> (classpath discovery)
    // 2. JPMS module descriptor `provides` declarations
    let mut provider_names: Vec<String> = Vec::new();

    // Source 1: META-INF/services file
    let resource_name = format!("META-INF/services/{iface_name}");
    if let Some(bytes) = ctx.find_resource(&resource_name) {
        let content = String::from_utf8_lossy(&bytes);
        for line in content.lines() {
            let name = line.split('#').next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                provider_names.push(name);
            }
        }
    }

    // Source 2: JPMS `provides` declarations from all registered modules.
    // The service_class is in slash format for module descriptors.
    let service_slash = iface_name.replace('.', "/");
    let module_providers = ctx.service_providers_from_modules(&service_slash);
    for mp in module_providers {
        // Module providers are in slash format; convert to dot for consistency.
        let dot_name = mp.replace('/', ".");
        if !provider_names.contains(&dot_name) {
            provider_names.push(dot_name);
        }
    }
    provider_names
}

pub fn register_s1_classloading(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;

    // =========================================================================
    // java.net.URLClassLoader
    // =========================================================================
    let ucl = "java/net/URLClassLoader";

    // Helper closure: extract filesystem paths from a URL[] array object
    // and register them with the VM classpath.
    fn s1_register_url_paths(ctx: &mut dyn NativeContext, url_arr_val: Value) {
        let paths = s1_url_array_to_fs_paths(ctx, url_arr_val);
        if !paths.is_empty() {
            ctx.register_dynamic_classpath(&paths);
        }
    }

    // URLClassLoader(URL[])
    r.register(ucl, "<init>", "([Ljava/net/URL;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let url_arr = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, url_arr);
        ctx.set_field(this, 1, Value::Object(None));
        s1_register_url_paths(ctx, url_arr);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // URLClassLoader(URL[], ClassLoader)
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_arr = args.get(1).copied().unwrap_or(Value::Object(None));
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, url_arr);
            ctx.set_field(this, 1, parent);
            s1_register_url_paths(ctx, url_arr);
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader(URL[], ClassLoader, URLStreamHandlerFactory)
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_arr = args.get(1).copied().unwrap_or(Value::Object(None));
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, url_arr);
            ctx.set_field(this, 1, parent);
            s1_register_url_paths(ctx, url_arr);
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader.newInstance(URL[]) → URLClassLoader
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;)Ljava/net/URLClassLoader;",
        |ctx, args| {
            // gc-common w13-d: the allocation can collect; the URL array is
            // stored into the new loader afterwards.
            let mut kept = [args.first().copied().unwrap_or(Value::Object(None))];
            let loader = keep_across(ctx, &mut kept, |ctx| {
                try_alloc_concurrent_synthetic(ctx, "java/net/URLClassLoader", 2)
            })?;
            let url_arr = kept[0];
            ctx.set_field(loader, 0, url_arr);
            ctx.set_field(loader, 1, Value::Object(None));
            s1_register_url_paths(ctx, url_arr);
            Ok(Some(Value::Object(Some(loader))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader.newInstance(URL[], ClassLoader) → URLClassLoader
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;",
        |ctx, args| {
            // gc-common w13-d: as the one-argument form above.
            let mut kept = [
                args.first().copied().unwrap_or(Value::Object(None)),
                args.get(1).copied().unwrap_or(Value::Object(None)),
            ];
            let loader = keep_across(ctx, &mut kept, |ctx| {
                try_alloc_concurrent_synthetic(ctx, "java/net/URLClassLoader", 2)
            })?;
            let [url_arr, parent] = kept;
            ctx.set_field(loader, 0, url_arr);
            ctx.set_field(loader, 1, parent);
            s1_register_url_paths(ctx, url_arr);
            Ok(Some(Value::Object(Some(loader))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader.loadClass(String) → Class
    r.register(
        ucl,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx
                .read_string(name_obj)
                .unwrap_or_default()
                .replace('.', "/");
            match ctx.ensure_class_initialized(&name) {
                Ok(cid) => {
                    let mirror = ctx.get_class_mirror(cid);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                Err(_) => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader.findClass(String) → Class  (same as loadClass)
    r.register(
        ucl,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx
                .read_string(name_obj)
                .unwrap_or_default()
                .replace('.', "/");
            match ctx.ensure_class_initialized(&name) {
                Ok(cid) => {
                    let mirror = ctx.get_class_mirror(cid);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                Err(_) => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader.getURLs() → URL[]
    r.register(ucl, "getURLs", "()[Ljava/net/URL;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // URLClassLoader.addURL(URL) — extend classpath with one more URL
    r.register(ucl, "addURL", "(Ljava/net/URL;)V", |ctx, args| {
        let url_val = args.get(1).copied().unwrap_or(Value::Object(None));
        if let Value::Object(Some(url_obj)) = url_val {
            let full_val = ctx.get_field(url_obj, 5); // URL_FIELD_FULL
            if let Value::Object(Some(s)) = full_val {
                if let Some(full) = ctx.read_string(s) {
                    if let Some(path) = s1_url_to_fs_path(&full) {
                        ctx.register_dynamic_classpath(&[path]);                    }                }            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // This exact URLClassLoader registration is installed after the generic
    // ClassLoader one. Keep it on the same parent-first implementation so a
    // URLClassLoader child can see @WithResource files supplied by its parent.
    r.register(
        ucl,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        crate::classloader::cl_get_resource_essential, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Use the same delegation path for stream lookup.
    r.register(
        ucl,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        crate::classloader::cl_get_resource_as_stream_essential, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // URLClassLoader.close() — IMPLEMENTED (was a no-op).
    //
    // Real `close()` shuts the `URLClassPath` so a closed loader stops finding
    // NEW classes and resources; classes it already defined stay defined
    // (`close()` unloads nothing). This synthetic loader opens no JarFiles of
    // its own — `<init>`/`addURL` hand the paths to
    // `ctx.register_dynamic_classpath` — so the whole of the observable contract
    // is retracting those roots, which is now possible:
    // `NativeContext::unregister_dynamic_classpath` reaches
    // `ClassPath::remove_path`, which use-counts each spec (a JAR handed to two
    // live loaders survives the first close) and never touches a startup
    // classpath root.
    //
    // Idempotent, per the javadoc: closing twice retracts once, because the
    // second call finds the use count already gone. Errors are not reported —
    // `close()` throws `IOException` only when a resource fails to close, and
    // there is nothing here that can fail.
    r.register(ucl, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let url_arr = ctx.get_field(this, 0);
        let paths = s1_url_array_to_fs_paths(ctx, url_arr);
        if !paths.is_empty() {
            ctx.unregister_dynamic_classpath(&paths);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // =========================================================================
    // java.util.ServiceLoader — real META-INF/services discovery
    //
    // Overrides the stubs registered in phase 53 and phase 63.
    // Layout: 2 fields — field 0 = Class mirror, field 1 = Object[] (lazy)
    // =========================================================================
    let sl = "java/util/ServiceLoader";

    // ServiceLoader.load(Class) → ServiceLoader
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            // gc-common w13-d: the allocation can collect; the mirror is
            // stored into the new loader afterwards.
            let mut kept = [args.first().copied().unwrap_or(Value::Object(None))];
            let obj = keep_across(ctx, &mut kept, |ctx| {
                try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2)
            })?;
            ctx.set_field(obj, 0, kept[0]);
            ctx.set_field(obj, 1, Value::Object(None)); // not yet loaded
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ServiceLoader.load(Class, ClassLoader) → ServiceLoader
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            // gc-common w13-d: the allocation can collect; the mirror is
            // stored into the new loader afterwards.
            let mut kept = [args.first().copied().unwrap_or(Value::Object(None))];
            let obj = keep_across(ctx, &mut kept, |ctx| {
                try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2)
            })?;
            ctx.set_field(obj, 0, kept[0]);
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ServiceLoader.loadInstalled(Class) → ServiceLoader
    r.register(
        sl,
        "loadInstalled",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            // gc-common w13-d: the allocation can collect; the mirror is
            // stored into the new loader afterwards.
            let mut kept = [args.first().copied().unwrap_or(Value::Object(None))];
            let obj = keep_across(ctx, &mut kept, |ctx| {
                try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2)
            })?;
            ctx.set_field(obj, 0, kept[0]);
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ServiceLoader.iterator() → Iterator<S>
    r.register(sl, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut arr = s1_service_loader_ensure_loaded(ctx, this);
        let len = ctx.array_length(arr);
        // Build a simple iterator backed by index over the array:
        // Iterator = 2-field: array=0, index=1
        // gc-common w13-d: the allocation can collect; `arr` is stored after.
        let itr = keep_one_across(ctx, &mut arr, |ctx| {
            try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2)
        })?;
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        // Register hasNext/next for ServiceLoader$Itr if not already
        let _ = len; // suppress unused
        Ok(Some(Value::Object(Some(itr))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // ServiceLoader$Itr.hasNext()
    r.register(
        "java/util/ServiceLoader$Itr",
        "hasNext",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let idx = match ctx.get_field(this, 1) {
                Value::Int(i) => i,
                _ => return Ok(Some(Value::Int(0))),
            };
            let len = ctx.array_length(arr) as i32;
            Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ServiceLoader$Itr.next()
    r.register(
        "java/util/ServiceLoader$Itr",
        "next",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let idx = match ctx.get_field(this, 1) {
                Value::Int(i) => i,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr) as i32;
            if idx >= len {
                return Ok(Some(Value::Object(None)));
            }
            let elem = ctx.get_array_element(arr, idx as usize);
            ctx.set_field(this, 1, Value::Int(idx + 1));
            Ok(Some(elem))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ServiceLoader.stream() → Stream<Provider<S>>
    r.register(sl, "stream", "()Ljava/util/stream/Stream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s1_service_loader_ensure_loaded(ctx, this);
        let len = ctx.array_length(arr);
        let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
        let s = p56_build_stream(ctx, elems, "java/util/stream/Stream")?;
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // ServiceLoader.findFirst() → Optional<S>
    r.register(sl, "findFirst", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut arr = s1_service_loader_ensure_loaded(ctx, this);
        // gc-common w13-d: the allocation can collect; `arr` is read after.
        let opt = keep_one_across(ctx, &mut arr, |ctx| {
            try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)
        })?;
        if ctx.array_length(arr) > 0 {
            let first = ctx.get_array_element(arr, 0);
            ctx.set_field(opt, 0, first);
        } else {
            ctx.set_field(opt, 0, Value::Object(None));
        }
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // ServiceLoader.reload() — clear cached services
    r.register(sl, "reload", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Object(None)); // clear cache
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(sl, "toString", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("ServiceLoader[]");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// =============================================================================
// Phase S.2: Real ByteBuffer + NIO Socket I/O
// =============================================================================
//
// ByteBuffer layout (6-field synthetic):
//   Field 0: Object (byte[]) — backing array
//   Field 1: Int             — position
//   Field 2: Int             — limit
//   Field 3: Int             — capacity
//   Field 4: Int             — mark (-1 = unset)
//   Field 5: Int             — byte order (0=BIG_ENDIAN, 1=LITTLE_ENDIAN)
//
// SocketChannel layout (5-field synthetic — supersedes p58 3-field):
//   Field 0: Int    — connected (0/1)
//   Field 1: Int    — open (0/1)
//   Field 2: Object — InetSocketAddress or null
//   Field 3: Int    — socket_id in SOCKET_REGISTRY (-1 = no OS socket)
//   Field 4: Int    — blocking (1=blocking, 0=non-blocking)
//
// ServerSocketChannel layout (5-field synthetic — supersedes p58 2-field):
//   Field 0: Int    — open (0/1)
//   Field 1: Int    — bound (0/1)
//   Field 2: Int    — listener_id in SOCKET_REGISTRY (-1 = unbound)
//   Field 3: Int    — port
//   Field 4: Int    — pending_stream_id (-1 = none; set by selector poll)
//
// Selector layout (3-field synthetic — supersedes p58 2-field):
//   Field 0: Int    — open (0/1)
//   Field 1: Object — Object[] of SelectionKey refs (all registered keys)
//   Field 2: Int    — count of entries in the keys array
//
// SelectionKey layout (4-field synthetic — supersedes p58 3-field):
//   Field 0: Object — channel (SocketChannel or ServerSocketChannel)
//   Field 1: Object — selector
//   Field 2: Int    — interestOps
//   Field 3: Int    — readyOps (updated by Selector.select())
// =============================================================================

// ---- Global socket registry ------------------------------------------------

pub struct SocketRegistry {
    pub next_id: i32,
    // Arc<TcpStream> (not bare TcpStream): a blocking read must run on the SAME
    // OS socket fd without holding the global registry lock, so callers clone
    // the cheap Arc under a short lock then do I/O on the shared `&TcpStream`
    // (std impls Read/Write for `&TcpStream`). A try_clone() duplicate fd is the
    // wrong tool here — on Windows a recv() blocked on a *duplicate* handle is
    // not woken by data that arrives after it blocked, only by FIN/close, which
    // wedges every request/response loopback (okhttp MockWebServer never
    // replies). Holding the lock across the blocking read instead deadlocks the
    // peer writer. Arc gives a same-fd handle that outlives the lock. (BUG-04)
    pub streams: HashMap<i32, Arc<TcpStream>>,
    pub listeners: HashMap<i32, TcpListener>,
    /// NEW-3: persistent UDP sockets backing `java.nio.channels.DatagramChannel`
    /// instances. The previous implementation re-created a `UdpSocket` on every
    /// `send`/`receive` call, which (a) wasted socket fds, (b) changed the local
    /// ephemeral port on every call, (c) made selector registration impossible.
    pub dgrams: HashMap<i32, UdpSocket>,
    /// NEW-13: persistent TLS streams backing `javax.net.ssl.SSLSocket` instances,
    /// unified with the plain-TCP registry so that SSLSocket and Socket share
    /// the same id space and close/shutdown semantics. Each entry couples the
    /// native-tls stream with the peer certificate DER bytes captured during
    /// the handshake so `SSLSession.getPeerCertificates()` can reconstruct a
    /// `java.security.cert.X509Certificate[]` later.
    pub tls_streams: HashMap<i32, TlsEntry>,
    /// gc-common w13-d: the VM (`NativeContext::vm_identity`) that opened each
    /// row of the four maps above, keyed by the same id. The maps are one per
    /// process, so without this a VM's teardown could not tell its own sockets
    /// from another VM's, and a disposed VM kept its ports bound and its
    /// descriptors open for the life of the host process
    /// (`common-w12d-servlet-os-socket-tables-outlive-their-vm`).
    ///
    /// Written by the `*_for_vm` allocators, [`s2_alloc_accepted_stream`]
    /// (an accepted stream belongs to its listener's VM) and
    /// [`s2_claim_for_vm`]. A row a TLS connector registers has no owner
    /// until its caller claims it. Read by
    /// [`forget_vm_s2_sockets`]. A close removes the socket row but not this
    /// one; [`s2_note_owner`] prunes such leftovers (amortised O(1)) and
    /// [`s2_next_free_id`] drops a leftover before its id is minted again, so
    /// a stale row never hands a new socket to the wrong VM.
    pub owners: HashMap<i32, usize>,
}

/// NEW-13: per-TLS-stream state captured during the handshake. The peer cert
/// chain is stored as DER bytes so it can be handed out repeatedly through
/// `SSLSession.getPeerCertificates()` without touching the live TLS stream.
pub struct TlsEntry {
    /// Per-stream mutex, deliberately NOT guarded by `s2_registry()`: a
    /// blocking TLS read/write must not hold the process-wide socket
    /// registry lock (see `s2_tls_read`'s doc comment).
    ///
    /// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Its two acquisitions
    /// (`s2_tls_read`, `s2_tls_write`) each hold it across exactly one
    /// `read`/`write` on the underlying stream and then `drop(guard)`; both
    /// release `s2_registry()` before taking it, which is the ordering the
    /// field comment above already required in prose. Nothing under the guard
    /// touches a `NativeContext`.
    pub stream: Arc<cratonvm_types::lock_order::OrderedPlMutex<TlsClientStream>>,
    /// A `try_clone`d handle on the same underlying socket, for fd-level
    /// operations (`shutdownInput/Output`, `set/getSoTimeout`) that must NOT
    /// wait on `stream`'s mutex — `shutdownInput` is exactly how a caller
    /// unblocks a peer parked in a blocking TLS read, so taking that mutex
    /// here would deadlock. `None` only if `try_clone` failed.
    pub raw: Option<TcpStream>,
    pub peer_host: String,
    pub peer_port: u16,
    /// T2.7.11: owned so real negotiated handshake values can live here
    /// instead of compile-time placeholders. Populated by `s2_tls_connect`
    /// from `TlsStream::negotiated_alpn()` plus connector bounds.
    pub negotiated_protocol: String,
    pub negotiated_cipher: String,
    /// T2.7.11: the ALPN protocol the server selected during the handshake
    /// (`None` if neither side offered ALPN). Surfaced as
    /// `SSLSocket.getApplicationProtocol()` on the Java side.
    pub negotiated_alpn: Option<String>,
    pub peer_cert_chain_der: Vec<Vec<u8>>,
}

pub enum TlsClientStream {
    Native(native_tls::TlsStream<TcpStream>),
    /// A raw SChannel stream, the Windows counterpart of [`Self::Openssl`] and
    /// for the same reason -- see [`s2_schannel_tls_connect_on`].
    #[cfg(windows)]
    Schannel(schannel::tls_stream::TlsStream<TcpStream>),
    /// A raw `openssl` stream. Two callers produce one: the legacy DSA bridge
    /// (`s2_legacy_dsa_tls_connect_on`) and — since
    /// `tls-client-captures-only-the-leaf` — the DEFAULT client path
    /// (`s2_openssl_tls_connect_on`). The variant was called `LegacyDsa` while
    /// the first was the only one; nothing downstream ever branched on which
    /// bridge built it, so the two share it rather than duplicating the
    /// read/write/shutdown arms.
    #[cfg(unix)]
    Openssl(openssl::ssl::SslStream<TcpStream>),
}

impl TlsClientStream {
    pub fn get_ref(&self) -> &TcpStream {
        match self {
            Self::Native(stream) => stream.get_ref(),
            #[cfg(unix)]
            Self::Openssl(stream) => stream.get_ref(),
            #[cfg(windows)]
            Self::Schannel(stream) => stream.get_ref(),
        }
    }
}

impl Default for SocketRegistry {
    fn default() -> Self {
        Self {
            next_id: 1,
            streams: HashMap::new(),
            listeners: HashMap::new(),
            dgrams: HashMap::new(),
            tls_streams: HashMap::new(),
            owners: HashMap::new(),
        }
    }
}

pub static SOCKET_REGISTRY: OnceLock<parking_lot::Mutex<SocketRegistry>> = OnceLock::new();

pub fn s2_registry() -> &'static parking_lot::Mutex<SocketRegistry> {
    SOCKET_REGISTRY.get_or_init(|| parking_lot::Mutex::new(SocketRegistry::default()))
}

/// Advance `next_id` to a value not currently in use by any of the four
/// socket tables. Pulled into a helper so every allocation site uses the
/// same collision-avoidance logic.
///
/// gc-common w13-d: `tls_streams` is checked too. The TLS connectors mint
/// their ids here, and before this a wrapped `next_id` could hand a new plain
/// socket the id of a live TLS stream. The id's leftover owner row (from a
/// closed socket that once held it) is dropped here, so the new row starts
/// unowned until its allocator says otherwise.
pub fn s2_next_free_id(reg: &mut SocketRegistry) -> i32 {
    let mut id = reg.next_id;
    // Skip 0 (reserved "invalid"), then skip any id that collides with an
    // existing entry in any of the tables.
    loop {
        if id == 0 {
            id = 1;
            continue;
        }
        if s2_id_is_live(reg, id) {
            id = id.checked_add(1).unwrap_or(1);
            continue;
        }
        break;
    }
    reg.next_id = id.checked_add(1).unwrap_or(1);
    reg.owners.remove(&id);
    id
}

/// Does `id` name a live row in any of the four socket tables?
pub fn s2_id_is_live(reg: &SocketRegistry, id: i32) -> bool {
    reg.streams.contains_key(&id)
        || reg.listeners.contains_key(&id)
        || reg.dgrams.contains_key(&id)
        || reg.tls_streams.contains_key(&id)
}

/// Record that `vm` opened row `id`, and prune the owner rows whose socket a
/// close has since removed.
///
/// The close natives (here and in `net_phase_e`, `phases_early`,
/// `net_channels`) remove only the socket row. Pruning when the owner table
/// has grown to twice the live rows keeps it bounded by the live socket count
/// at amortised O(1) per allocation, without touching any of those close
/// sites.
pub fn s2_note_owner(reg: &mut SocketRegistry, id: i32, vm: usize) {
    reg.owners.insert(id, vm);
    let live = reg.streams.len() + reg.listeners.len() + reg.dgrams.len() + reg.tls_streams.len();
    if reg.owners.len() > 64 && reg.owners.len() > 2 * live {
        s2_prune_owner_rows(reg);
    }
}

/// Drop every owner row whose id no longer names a live socket.
pub fn s2_prune_owner_rows(reg: &mut SocketRegistry) {
    let mut owners = std::mem::take(&mut reg.owners);
    let live: &SocketRegistry = reg;
    owners.retain(|id, _| s2_id_is_live(live, *id));
    reg.owners = owners;
}

/// Register a connected stream that VM `vm` opened. [`forget_vm_s2_sockets`]
/// closes it when that VM is torn down.
pub fn s2_alloc_stream_for_vm(vm: usize, stream: TcpStream) -> i32 {
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.streams.insert(id, Arc::new(stream));
    s2_note_owner(&mut reg, id, vm);
    id
}

/// Register a listener that VM `vm` bound. [`forget_vm_s2_sockets`] closes it
/// (releasing the port) when that VM is torn down.
pub fn s2_alloc_listener_for_vm(vm: usize, listener: TcpListener) -> i32 {
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.listeners.insert(id, listener);
    s2_note_owner(&mut reg, id, vm);
    id
}

/// NEW-3: register a persistent UDP socket with the shared registry so that
/// `DatagramChannel` operations reuse the same fd across send/receive/read/
/// write calls and so that the Selector can poll its readiness.
///
/// gc-common w13-d / w14-b: the row is owned by VM `vm`, so
/// [`forget_vm_s2_sockets`] closes it at that VM's teardown. The caller is
/// `net_channels`' `DatagramChannel` `ensure_bound`.
pub fn s2_alloc_dgram_for_vm(vm: usize, dgram: UdpSocket) -> i32 {
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.dgrams.insert(id, dgram);
    s2_note_owner(&mut reg, id, vm);
    id
}

/// Register a stream accepted from listener `lid`. It belongs to the VM that
/// owns the listener, so an accept site needs no `ctx`. A listener with no
/// owner (legacy allocator) or one closed during the accept gives an unowned
/// stream.
pub fn s2_alloc_accepted_stream(lid: i32, stream: TcpStream) -> i32 {
    let mut reg = s2_registry().lock();
    s2_insert_accepted_stream(&mut reg, lid, stream)
}

pub fn s2_insert_accepted_stream(reg: &mut SocketRegistry, lid: i32, stream: TcpStream) -> i32 {
    let owner = reg.owners.get(&lid).copied();
    let id = s2_next_free_id(reg);
    reg.streams.insert(id, Arc::new(stream));
    if let Some(vm) = owner {
        s2_note_owner(reg, id, vm);
    }
    id
}

/// Record that VM `vm` owns the already-registered row `id`. For the
/// allocation paths that take no VM, chiefly the TLS connectors
/// (`s2_tls_connect*`), whose callers claim the id they return. A no-op for an
/// id that names no live row (one closed in between).
///
/// gc-common w14-b: an id at or above [`RUSTLS_SOCK_ID_BASE`] names a row of
/// the rustls registry, and is forwarded to `t27_tls::rustls_claim_for_vm`
/// (the same split `s2_tls_read` makes), so a caller holding either kind of
/// TLS id claims it with this one call. Ids in the pending ranges below that
/// base name no socket row yet and are ignored.
///
/// Its callers: `ssl_security::new13_connect_and_handshake_on` (every client
/// connector arm, gc-common w14-b) and `net_phase_e`'s `createSocket` client
/// arms (lane A14).
pub fn s2_claim_for_vm(vm: usize, id: i32) {
    if id >= RUSTLS_SOCK_ID_BASE {
        crate::t27_tls::rustls_claim_for_vm(vm, id - RUSTLS_SOCK_ID_BASE);
        return;
    }
    let mut reg = s2_registry().lock();
    if s2_id_is_live(&reg, id) {
        s2_note_owner(&mut reg, id, vm);
    }
}

/// gc-common w13-d: close and drop every socket row VM `vm` opened, its TLS
/// read-ahead, and its `Selector` wakeup channels. Called from
/// `lib.rs::forget_vm_native_root_stores` at VM teardown.
///
/// Dropping a `TcpListener` / `UdpSocket` closes it, which releases its port;
/// the last `Arc<TcpStream>` does the same for a stream. Teardown runs after
/// the VM's threads have stopped, so no thread should still hold a cloned
/// `Arc` or a `try_clone`d handle. In case one does, each stream (and each TLS
/// stream's `raw` duplicate) is shut down first, as `s2_tls_close` does, so a
/// straggler parked in a read wakes on Unix rather than holding the socket
/// open. Other VMs' rows, and unowned rows, are left alone.
///
/// gc-common w14-b: the rustls listeners and streams (ids at or above
/// [`RUSTLS_SOCK_ID_BASE`]) go too, through
/// `t27_tls::forget_vm_rustls_sockets`, with their read-ahead.
///
/// These tables hold OS handles and Rust data only, no heap references, so no
/// collector scans them and there is no GC hook to scope.
pub fn forget_vm_s2_sockets(vm: usize) {
    let mut ids: Vec<i32> = Vec::new();
    let mut streams: Vec<Arc<TcpStream>> = Vec::new();
    let mut listeners: Vec<TcpListener> = Vec::new();
    let mut dgrams: Vec<UdpSocket> = Vec::new();
    let mut tls: Vec<TlsEntry> = Vec::new();
    {
        let mut reg = s2_registry().lock();
        reg.owners.retain(|id, owner| {
            if *owner == vm {
                ids.push(*id);
                false
            } else {
                true
            }
        });
        for id in &ids {
            if let Some(s) = reg.streams.remove(id) {
                streams.push(s);
            }
            if let Some(l) = reg.listeners.remove(id) {
                listeners.push(l);
            }
            if let Some(d) = reg.dgrams.remove(id) {
                dgrams.push(d);
            }
            if let Some(t) = reg.tls_streams.remove(id) {
                tls.push(t);
            }
        }
    }
    // gc-common w14-b: the rustls registry shares this id space above
    // `RUSTLS_SOCK_ID_BASE`; its rows go with the VM's here too
    // (`common-w13d-rustls-and-plain-socket-registries-outlive-their-vm`).
    let rustls_ids = crate::t27_tls::forget_vm_rustls_sockets(vm);
    // gc-common w14-b handoff: the parked zero-arg `SSLSocket` connections
    // and their staged trust scopes.
    crate::phases_late::ssl_security::forget_vm_parked_tls_sockets(vm);
    if !ids.is_empty() || !rustls_ids.is_empty() {
        let mut readahead = tls_readahead().lock();
        for id in &ids {
            readahead.remove(id);
        }
        for id in &rustls_ids {
            readahead.remove(&(RUSTLS_SOCK_ID_BASE + id));
        }
    }
    // Outside the registry lock: a shutdown is a syscall.
    for s in &streams {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    for t in &tls {
        if let Some(raw) = t.raw.as_ref() {
            let _ = raw.shutdown(std::net::Shutdown::Both);
        }
    }
    drop(streams);
    drop(listeners);
    drop(dgrams);
    drop(tls);
    selector_wakeups()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
}

/// NEW-13: connect a TLS stream to `host:port`, run the handshake, and
/// register it with the shared socket registry. Returns the registry id or a
/// structured error describing why the handshake failed.
///
/// The `connector` argument is already-configured (cert validation, SNI, ALPN
/// policy, protocol bounds). On success, the entry stores the negotiated
/// protocol / cipher strings and the peer cert chain DER bytes so that
/// `SSLSession.getCipherSuite()` / `getProtocol()` / `getPeerCertificates()`
/// can reflect the *actual* handshake outcome instead of placeholder values.
pub fn s2_tls_connect(
    connector: &native_tls::TlsConnector,
    host: &str,
    port: u16,
) -> Result<i32, TlsConnectFailure> {
    let addr = format!("{}:{}", host, port);
    let tcp = TcpStream::connect(&addr).map_err(TlsConnectFailure::Tcp)?;
    s2_tls_connect_on(connector, host, port, tcp)
}

/// Why a client TLS connect attempt failed, kept apart so the caller can raise
/// the exception JSSE raises.
///
/// `SSLSocketFactory.createSocket` reports a refused/unroutable TCP connect as
/// a plain `IOException` and a REJECTED HANDSHAKE as
/// `javax.net.ssl.SSLHandshakeException` — callers `catch` on that type (see
/// `ensure_layered_handshake_started`'s note about tests asserting on the JSSE
/// type for an intentionally-rejected connection). Flattening both into one
/// `IOException` with a `format!`ed message, which is what this path did,
/// makes the two indistinguishable to a `catch` block.
pub enum TlsConnectFailure {
    /// The TCP connection could not be established.
    Tcp(std::io::Error),
    /// TCP succeeded; the TLS handshake did not. Carries the backend's own
    /// description (for OpenSSL, the certificate-verification error).
    Handshake(String),
}

impl std::fmt::Display for TlsConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsConnectFailure::Tcp(e) => write!(f, "{e}"),
            TlsConnectFailure::Handshake(m) => f.write_str(m),
        }
    }
}

/// [`s2_tls_connect`] over an ALREADY-CONNECTED stream.
///
/// `SSLSocket.connect(SocketAddress)` establishes the TCP connection and the
/// handshake runs later (see [`PENDING_CONNECT_SOCK_ID_BASE`]); the connection
/// it opened is the one the handshake must run on. Opening a second one
/// instead is observable to the peer: a server that accepts one connection per
/// client accepts the FIRST (which carries no ClientHello, so its handshake
/// reads EOF) and is no longer in `accept()` when the second arrives, which
/// then waits out the 30 s read timeout below and reports
/// `HandshakeError::WouldBlock` — "the handshake process was interrupted",
/// ~30 s after a rejection the peer had already answered.
pub fn s2_tls_connect_on(
    connector: &native_tls::TlsConnector,
    host: &str,
    port: u16,
    tcp: TcpStream,
) -> Result<i32, TlsConnectFailure> {
    // Reasonable defaults: non-infinite read/write timeouts so a hung peer
    // never deadlocks the JVM thread calling `SSLSocket.getInputStream().read`.
    // An established socket's application timeout is already its policy.
    // Use the connector's handshake fallback only when no timeout was set.
    if tcp.read_timeout().ok().flatten().is_none() {
        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    }
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_nonblocking(false);

    let tls_stream = connector
        .connect(host, tcp)
        .map_err(|e| TlsConnectFailure::Handshake(format!("TLS handshake failed: {}", e)))?;

    // T2.7.11: native-tls 0.2's public `TlsStream` API does not expose the
    // server-selected ALPN protocol on all backends (it is absent on 0.2's
    // SChannel path and gated behind a private field on the OpenSSL path).
    // The rustls-backed server/client path in `t27_tls` captures the real
    // selection instead. For this client path we leave the field empty when
    // we cannot prove a specific selection.
    let negotiated_alpn: Option<String> = None;

    // native-tls 0.2 does not expose the negotiated TLS version or ciphersuite
    // through a stable API (the SChannel / SecureTransport / OpenSSL backends
    // each store it differently). The connector is configured with TLS 1.2 as
    // the minimum and TLS 1.3 is preferred by every backend we build against,
    // so reporting "TLSv1.3" / "TLS_AES_128_GCM_SHA256" is the highest-fidelity
    // value we can return without a backend break. The rustls-backed server
    // path in `t27_tls` reports the real values because rustls exposes them.
    let negotiated_protocol = String::from("TLSv1.3");
    let negotiated_cipher = String::from("TLS_AES_128_GCM_SHA256");

    // Capture the peer's leaf certificate DER bytes. native-tls's public API
    // only exposes the leaf via `peer_certificate()`; there is no chain
    // accessor.
    //
    // THE REASONING THIS COMMENT USED TO CARRY WAS WRONG, and worth recording
    // because of HOW it went wrong. It said the full chain "is validated
    // internally by the backend before `connect` returns, which is why we can
    // rely on a single-element chain here without weakening security." That
    // was true when written — the backend WAS the verifier and the captured
    // leaf was only ever informational. It stopped being true when the
    // `java_tm_key` path was added to `new13_connect_and_handshake_on`: that
    // path disables native verification precisely so an application
    // TrustManager can decide, and it consumes THIS vector. A premise was
    // invalidated by a later change to a different function and nothing
    // re-checked it. MEASURED consequence: 20 of 20 live public sites
    // rejected, every one at `chainLen=1`.
    //
    // On Unix the default client path no longer comes through here at all —
    // see `s2_openssl_tls_connect_on`, which asks OpenSSL for the whole chain.
    // This arm is what remains: Windows, and `CRATONVM_TLS_OPENSSL_CLIENT=0`.
    let mut peer_cert_chain_der: Vec<Vec<u8>> = Vec::new();
    match tls_stream.peer_certificate() {
        Ok(Some(cert)) => match cert.to_der() {
            Ok(der) => peer_cert_chain_der.push(der),
            Err(e) => {
                return Err(TlsConnectFailure::Handshake(format!(
                    "peer certificate DER encode failed: {}",
                    e
                )));
            }
        },
        Ok(None) => {
            // No peer cert presented (e.g. PSK / anonymous ciphersuite).
            // Leave the chain empty; getPeerCertificates will throw
            // SSLPeerUnverifiedException.
        }
        Err(e) => {
            return Err(TlsConnectFailure::Handshake(format!(
                "peer certificate query failed: {}",
                e
            )));
        }
    }

    let raw = tls_stream.get_ref().try_clone().ok();
    let entry = TlsEntry {
        stream: Arc::new(cratonvm_types::lock_order::OrderedPlMutex::new(
            TlsClientStream::Native(tls_stream),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )),
        raw,
        peer_host: host.to_string(),
        peer_port: port,
        negotiated_protocol,
        negotiated_cipher,
        negotiated_alpn,
        peer_cert_chain_der,
    };

    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.tls_streams.insert(id, entry);
    Ok(id)
}

/// Connect using a per-connection OpenSSL policy for a legacy DSA identity.
/// The caller only selects this after recognizing a configured DSA trust root;
/// certificate validation still happens immediately afterward through the
/// Java TrustManager captured from the owning SSLContext.
#[cfg(unix)]
pub fn s2_legacy_dsa_tls_connect(
    host: &str,
    port: u16,
    trust_root_ders: &[Vec<u8>],
) -> Result<i32, TlsConnectFailure> {
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr).map_err(TlsConnectFailure::Tcp)?;
    s2_legacy_dsa_tls_connect_on(host, port, trust_root_ders, tcp)
}

/// [`s2_legacy_dsa_tls_connect`] over an ALREADY-CONNECTED stream — see
/// [`s2_tls_connect_on`] for why the deferred-handshake path must reuse the
/// connection `SSLSocket.connect` opened rather than open a second one.
#[cfg(unix)]
pub fn s2_legacy_dsa_tls_connect_on(
    host: &str,
    port: u16,
    trust_root_ders: &[Vec<u8>],
    tcp: TcpStream,
) -> Result<i32, TlsConnectFailure> {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    use openssl::x509::{store::X509StoreBuilder, X509VerifyResult, X509};
    let hs = |e: &dyn std::fmt::Display| TlsConnectFailure::Handshake(e.to_string());
    // An established socket's application timeout is already its policy.
    // Use the connector's handshake fallback only when no timeout was set.
    if tcp.read_timeout().ok().flatten().is_none() {
        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    }
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(|e| hs(&e))?;
    builder.set_security_level(0);
    builder
        .set_cipher_list("ALL:@SECLEVEL=0")
        .map_err(|e| hs(&e))?;
    let mut roots = X509StoreBuilder::new().map_err(|e| hs(&e))?;
    for der in trust_root_ders {
        let cert = X509::from_der(der).map_err(|e| hs(&e))?;
        roots.add_cert(cert).map_err(|e| hs(&e))?;
    }
    builder
        .set_verify_cert_store(roots.build())
        .map_err(|e| hs(&e))?;
    // Spring Boot's historical embedded-LDAP fixture explicitly trusts a
    // self-signed DSA certificate whose validity window ended in 2017.  The
    // JVM trust-manager shim accepts that explicit anchor; retain normal
    // chain verification but mirror that compatibility behavior for the
    // one expiration error in this legacy-DSS bridge.
    builder.set_verify_callback(SslVerifyMode::PEER, |verified, store| {
        verified || store.error() == unsafe { X509VerifyResult::from_raw(10) }
    });
    // A plain JSSE SSLSocket validates the peer chain but does not perform
    // hostname verification unless the caller sets an endpoint-identification
    // algorithm in SSLParameters.  UnboundID connects its in-memory LDAPS
    // server via 127.0.0.1 while the test certificate has no matching IP SAN.
    let connector = builder.build();
    let mut connection = connector.configure().map_err(|e| hs(&e))?;
    connection.set_verify_hostname(false);
    let stream = connection
        .connect(host, tcp)
        .map_err(|e| TlsConnectFailure::Handshake(format!("legacy DSA TLS handshake: {e}")))?;
    let peer_cert_chain_der = openssl_peer_chain_der(stream.ssl()).map_err(|e| hs(&e))?;
    let raw = stream.get_ref().try_clone().ok();
    let entry = TlsEntry {
        stream: Arc::new(cratonvm_types::lock_order::OrderedPlMutex::new(
            TlsClientStream::Openssl(stream),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )),
        raw,
        peer_host: host.to_string(),
        peer_port: port,
        negotiated_protocol: "TLSv1.2".to_string(),
        negotiated_cipher: "UNKNOWN".to_string(),
        negotiated_alpn: None,
        peer_cert_chain_der,
    };
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.tls_streams.insert(id, entry);
    Ok(id)
}

/// The certificate SECURITY LEVEL the default client connector runs at.
///
/// OpenSSL's own default is 2, which requires a >= 2048-bit RSA key and
/// refuses a SHA-1 signature anywhere in the chain. The JDK's equivalent,
/// `jdk.certpath.disabledAlgorithms`, draws its line at 1024 bits. That gap is
/// not theoretical -- MEASURED (`WeakChainProbe`, against an `openssl
/// s_server` presenting a 1024-bit RSA leaf signed by a 1024-bit CA, with a
/// JDK image whose `cacerts` trusts that CA):
///
/// ```text
/// HOTSPOT   HANDSHAKE-OK  328 ms
/// CRATONVM  REFUSED        60 ms   ... (EE certificate key too weak)
/// ```
///
/// Level 1 is OpenSSL's 80-bit tier: RSA/DSA/DH >= 1024, ECC >= 160, SHA-1
/// permitted -- i.e. the JDK's own floor. It is NOT a blanket relaxation of
/// the posture: the connector still pins a TLS 1.2 minimum of its own, so the
/// SSLv3/TLS1.0 suites level 1 would otherwise readmit stay out.
#[cfg(unix)]
pub const CLIENT_SECURITY_LEVEL: u32 = 1;

/// Everything `new13_build_connector` expresses through
/// `native_tls::TlsConnectorBuilder`, restated for a raw
/// `openssl::ssl::SslConnector`.
///
/// The swap exists because native-tls 0.2 cannot express two things this VM
/// needs, and no amount of configuration on its side will make it:
///
/// * the peer's FULL certificate chain. `TlsStream::peer_certificate()` is the
///   LEAF and there is no chain accessor, so an application `TrustManager` --
///   which this VM correctly makes the ONLY verifier -- was handed a
///   one-element chain and could not build a path to any root. MEASURED across
///   20 public sites: 20 rejections at `chainLen=1`, against 20 acceptances at
///   2-4 on HotSpot.
/// * the certificate security level. See [`CLIENT_SECURITY_LEVEL`].
#[cfg(unix)]
pub struct OpensslClientConfig {
    /// Trust anchors (DER) to configure on the connector.
    pub roots: Vec<Vec<u8>>,
    /// `true` = `roots` REPLACE the platform set (JSSE's rule for a trust
    /// store the application named, and for the JDK's own `cacerts`);
    /// `false` = they are ADDED to it, which is what a per-`SSLContext` custom
    /// anchor set has always done here.
    pub replace_roots: bool,
    /// Stand OpenSSL's verifier (and its hostname check) DOWN: the caller is
    /// the verifier and runs immediately after connect, fail-closed. Mirrors
    /// `danger_accept_invalid_certs` + `danger_accept_invalid_hostnames`.
    pub skip_verify: bool,
    /// Pin the ceiling to TLS 1.2, for a version-specific
    /// `SSLContext.getInstance("TLSv1.2")`.
    pub max_tls12: bool,
}

/// The peer's certificate chain as DER, leaf first.
///
/// `SSL_get_peer_cert_chain` on a CLIENT includes the peer's own certificate
/// (on a server it does not -- the asymmetry is OpenSSL's, and this is only
/// ever called on client streams). The `peer_certificate()` fallback is not
/// belt-and-braces: on a RESUMED session the peer sends no Certificate
/// message, so the chain is absent while the cached leaf is still there, and
/// without the fallback a resumed connection would report ZERO certificates
/// where it used to report one.
#[cfg(unix)]
pub fn openssl_peer_chain_der(
    ssl: &openssl::ssl::SslRef,
) -> Result<Vec<Vec<u8>>, openssl::error::ErrorStack> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    if let Some(chain) = ssl.peer_cert_chain() {
        for cert in chain {
            out.push(cert.to_der()?);
        }
    }
    if out.is_empty() {
        if let Some(leaf) = ssl.peer_certificate() {
            out.push(leaf.to_der()?);
        }
    }
    Ok(out)
}

/// [`s2_openssl_tls_connect_on`], opening the connection here.
#[cfg(unix)]
pub fn s2_openssl_tls_connect(
    cfg: &OpensslClientConfig,
    host: &str,
    port: u16,
) -> Result<i32, TlsConnectFailure> {
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr).map_err(TlsConnectFailure::Tcp)?;
    s2_openssl_tls_connect_on(cfg, host, port, tcp)
}

/// The default `SSLSocket` client bridge, over a raw `openssl::SslConnector`.
///
/// Drop-in for [`s2_tls_connect_on`]: same timeouts, same SNI, same hostname
/// verification, same TLS 1.2 floor, same registry entry shape. What differs
/// is only what [`OpensslClientConfig`] documents -- the full chain and the
/// security level -- plus the negotiated protocol / cipher / ALPN, which this
/// backend can actually be asked for instead of being reported from a
/// compile-time constant.
///
/// `SslConnector::builder` already loads the platform roots
/// (`SSL_CTX_set_default_verify_paths`) and `configure()` already turns on SNI
/// and hostname verification, so the unconfigured shape here is native-tls's
/// shape, not a weaker one.
#[cfg(unix)]
pub fn s2_openssl_tls_connect_on(
    cfg: &OpensslClientConfig,
    host: &str,
    port: u16,
    tcp: TcpStream,
) -> Result<i32, TlsConnectFailure> {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};
    use openssl::x509::{store::X509StoreBuilder, X509};

    let hs = |e: &dyn std::fmt::Display| TlsConnectFailure::Handshake(e.to_string());
    // Same 30 s read/write floor `s2_tls_connect_on` sets, and for the same
    // reason: a hung peer must not deadlock the JVM thread that called
    // `SSLSocket.getInputStream().read`.
    // An established socket's application timeout is already its policy.
    // Use the connector's handshake fallback only when no timeout was set.
    if tcp.read_timeout().ok().flatten().is_none() {
        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    }
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_nonblocking(false);

    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(|e| hs(&e))?;
    builder.set_security_level(CLIENT_SECURITY_LEVEL);
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(|e| hs(&e))?;
    if cfg.max_tls12 {
        builder
            .set_max_proto_version(Some(SslVersion::TLS1_2))
            .map_err(|e| hs(&e))?;
    }
    if !cfg.roots.is_empty() {
        if cfg.replace_roots {
            let mut store = X509StoreBuilder::new().map_err(|e| hs(&e))?;
            let mut added = 0usize;
            for der in &cfg.roots {
                match X509::from_der(der) {
                    Ok(cert) => {
                        // One unparseable anchor must not sink the whole
                        // connector -- `new13_build_connector` logs and
                        // continues. But a REPLACING root set that lost every
                        // anchor that way would trust nothing at all while
                        // still looking configured, so the count is checked.
                        if store.add_cert(cert).is_ok() {
                            added += 1;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            target: "servlet::tls",
                            "openssl client: skipping unparseable trust anchor DER: {e}"
                        );
                    }
                }
            }
            if added == 0 {
                return Err(TlsConnectFailure::Handshake(
                    "no usable trust anchor in the configured trust store".to_string(),
                ));
            }
            builder
                .set_verify_cert_store(store.build())
                .map_err(|e| hs(&e))?;
        } else {
            let store = builder.cert_store_mut();
            for der in &cfg.roots {
                match X509::from_der(der) {
                    Ok(cert) => {
                        let _ = store.add_cert(cert);
                    }
                    Err(e) => {
                        tracing::debug!(
                            target: "servlet::tls",
                            "openssl client: skipping unparseable custom trust anchor DER: {e}"
                        );
                    }
                }
            }
        }
    }
    if cfg.skip_verify {
        builder.set_verify(SslVerifyMode::NONE);
    }
    let connector = builder.build();
    let mut connection = connector.configure().map_err(|e| hs(&e))?;
    if cfg.skip_verify {
        connection.set_verify_hostname(false);
    }
    let stream = connection
        .connect(host, tcp)
        .map_err(|e| TlsConnectFailure::Handshake(format!("TLS handshake failed: {e}")))?;

    let peer_cert_chain_der = openssl_peer_chain_der(stream.ssl()).map_err(|e| hs(&e))?;
    // The values native-tls forced this path to hard-code. `version_str`
    // already spells JSSE's names ("TLSv1.3"/"TLSv1.2"), and a cipher's
    // STANDARD name is the IANA/JSSE one: at TLS 1.3 it coincides with
    // OpenSSL's own ("TLS_AES_128_GCM_SHA256"), at TLS 1.2 it does not
    // ("ECDHE-RSA-AES128-GCM-SHA256" vs
    // "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256").
    let negotiated_protocol = stream.ssl().version_str().to_string();
    let negotiated_cipher = stream
        .ssl()
        .current_cipher()
        .map(|c| c.standard_name().unwrap_or_else(|| c.name()).to_string())
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .ssl()
        .selected_alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned());

    let raw = stream.get_ref().try_clone().ok();
    let entry = TlsEntry {
        stream: Arc::new(cratonvm_types::lock_order::OrderedPlMutex::new(
            TlsClientStream::Openssl(stream),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )),
        raw,
        peer_host: host.to_string(),
        peer_port: port,
        negotiated_protocol,
        negotiated_cipher,
        negotiated_alpn,
        peer_cert_chain_der,
    };
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.tls_streams.insert(id, entry);
    Ok(id)
}

/// [`OpensslClientConfig`]'s Windows counterpart, minus one field.
///
/// There is no `replace_roots` here, and its absence is the design rather than
/// an omission. On Unix `set_verify_cert_store` genuinely REPLACES the anchor
/// set, so JSSE's "this store instead of cacerts" is expressible. SChannel has
/// no such switch: the closest thing native-tls does for
/// `disable_built_in_roots` is to run the platform's normal verification and
/// then REJECT anything whose built chain does not touch a configured root.
/// That is a narrowing, not a replacement -- it can only refuse chains the
/// platform accepted, never accept one it did not. Applying it to the JDK's
/// `cacerts` would therefore refuse any site whose root the Windows store
/// lacks and cacerts has, which is a divergence from HotSpot in the direction
/// of breaking working connections.
///
/// So this connector changes ONE thing about the Windows path: the chain it
/// captures. Every trust decision is the decision native-tls was already
/// making, including the two stand-downs. The anchor-set question (Windows
/// root store vs `cacerts`) is real and is NOT answered here; it is a
/// different measurement on a different page.
#[cfg(windows)]
pub struct SchannelClientConfig {
    /// Anchors to ADD to the platform set, as native-tls has always done here.
    pub roots: Vec<Vec<u8>>,
    /// Stand SChannel's verifier and hostname check down: the caller is the
    /// verifier and runs immediately after connect, fail-closed.
    pub skip_verify: bool,
    /// Pin the ceiling to TLS 1.2, for `SSLContext.getInstance("TLSv1.2")`.
    pub max_tls12: bool,
}


/// Order a certificate SET into a path: `leaf` first, then whichever member of
/// `pool` issued the one before it, and so on. `names` yields
/// `(subject, issuer)` for a member.
///
/// Not `#[cfg(windows)]`, and not because it might be wanted elsewhere -- so
/// that its TEST runs everywhere. The logic is about a subject/issuer graph
/// and has nothing platform-specific in it; only its caller does.
///
/// Leftovers are APPENDED, never dropped. A peer may legitimately send a
/// certificate that is not on the path to the anchor this VM will pick -- a
/// cross-certificate is the usual case -- and `x509_manager::validate_chain`
/// builds its own path from the whole set. A certificate it never sees is one
/// it cannot build with.
pub fn order_chain_from_leaf<F>(leaf: Vec<u8>, mut pool: Vec<Vec<u8>>, names: F) -> Vec<Vec<u8>>
where
    F: Fn(&[u8]) -> Option<(Vec<u8>, Vec<u8>)>,
{
    let mut out = vec![leaf];
    loop {
        let issuer_of_last = match names(out.last().expect("chain is never empty")) {
            Some((_, issuer)) => issuer,
            None => break,
        };
        // A self-issued certificate is the end of the path: continuing past it
        // would loop on itself.
        let next = pool.iter().position(|der| {
            names(der)
                .map(|(subject, issuer)| subject == issuer_of_last && subject != issuer)
                .unwrap_or(false)
        });
        match next {
            Some(i) => out.push(pool.remove(i)),
            None => break,
        }
    }
    out.extend(pool);
    out
}

/// The peer's certificate chain as DER, leaf first, from an SChannel stream.
///
/// Two Windows facts do the work. `peer_certificate()` is the LEAF, and the
/// `CertContext` it returns carries an attached store -- SChannel's own words
/// for it are "a certificate store containing any intermediate certificates
/// provided by the remote sender" -- reachable as `CertContext::cert_store()`.
/// So the chain is there; `native_tls` simply never re-exports the stream that
/// owns it, which is the whole reason this function exists.
///
/// WHY THE PATH IS WALKED. `SSLSession.getPeerCertificates()` is ordered --
/// the peer's own certificate, then each issuer. On Unix that is free: OpenSSL
/// hands back a LIST, in the order the peer sent it. Here the source is a
/// `CertStore`, which is a SET; `certs()` enumerates it in whatever order the
/// store holds, and Windows promises nothing about that order.
///
/// MEASURED, and worth stating plainly rather than dressing up: with the walk
/// SKIPPED, live SChannel already returned leaf-first on all six hosts of
/// `PeerChainOrderProbe`, output identical to the walk's. So this is not a
/// repair of an observed defect -- it is the removal of a dependence on an
/// order the platform does not contract. The unit tests above `order_chain_from_leaf`
/// are what exercise it, since the live store declines to.
#[cfg(windows)]
pub fn schannel_peer_chain_der(stream: &schannel::tls_stream::TlsStream<TcpStream>) -> Vec<Vec<u8>> {
    let leaf_ctx = match stream.peer_certificate() {
        Ok(c) => c,
        // No peer certificate (PSK / anonymous suite). An empty chain is the
        // honest answer; `getPeerCertificates` throws
        // SSLPeerUnverifiedException on it, which is JSSE's behaviour.
        Err(_) => return Vec::new(),
    };
    let leaf = leaf_ctx.to_der().to_vec();
    let mut pool: Vec<Vec<u8>> = Vec::new();
    if let Some(store) = leaf_ctx.cert_store() {
        for cert in store.certs() {
            let der = cert.to_der().to_vec();
            if der != leaf && !pool.contains(&der) {
                pool.push(der);
            }
        }
    }
    order_chain_from_leaf(leaf, pool, |der| {
        crate::x509_manager::parse_certificate(der)
            .ok()
            .map(|p| (p.subject_der, p.issuer_der))
    })
}

/// [`s2_schannel_tls_connect_on`], opening the connection here.
#[cfg(windows)]
pub fn s2_schannel_tls_connect(
    cfg: &SchannelClientConfig,
    host: &str,
    port: u16,
) -> Result<i32, TlsConnectFailure> {
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr).map_err(TlsConnectFailure::Tcp)?;
    s2_schannel_tls_connect_on(cfg, host, port, tcp)
}

/// The default `SSLSocket` client bridge on Windows, over the `schannel` crate
/// directly instead of through `native_tls`.
///
/// The backend does not change -- native-tls IS SChannel here, and its
/// `imp/schannel.rs` is a thin wrapper over exactly the calls below. What
/// changes is that the stream stays in reach, so the peer's chain can be read
/// off it (see [`schannel_peer_chain_der`]). Everything the wrapper configured
/// is configured here: the protocol range, SNI, hostname verification, the
/// caller-supplied roots, and the two stand-downs.
///
#[cfg(windows)]
pub fn s2_schannel_tls_connect_on(
    cfg: &SchannelClientConfig,
    host: &str,
    port: u16,
    tcp: TcpStream,
) -> Result<i32, TlsConnectFailure> {
    use schannel::cert_context::CertContext;
    use schannel::cert_store::{CertAdd, Memory};
    use schannel::schannel_cred::{Direction, Protocol, SchannelCred};
    use schannel::tls_stream;

    // Same 30 s floor the other two bridges set, for the same reason.
    // An established socket's application timeout is already its policy.
    // Use the connector's handshake fallback only when no timeout was set.
    if tcp.read_timeout().ok().flatten().is_none() {
        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    }
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));

    let protocols: &[Protocol] = if cfg.max_tls12 {
        &[Protocol::Tls12]
    } else {
        &[Protocol::Tls12, Protocol::Tls13]
    };
    let cred = SchannelCred::builder()
        .enabled_protocols(protocols)
        .acquire(Direction::Outbound)
        .map_err(|e| TlsConnectFailure::Handshake(format!("SChannel credentials: {e}")))?;

    let mut roots = Memory::new()
        .map_err(|e| TlsConnectFailure::Handshake(format!("SChannel root store: {e}")))?
        .into_store();
    for der in &cfg.roots {
        match CertContext::new(der) {
            Ok(cert) => {
                let _ = roots.add_cert(&cert, CertAdd::ReplaceExisting);
            }
            Err(e) => {
                // One unparseable anchor must not sink the connector, which is
                // what `new13_build_connector` does with the same input.
                tracing::debug!(
                    target: "servlet::tls",
                    "schannel client: skipping unparseable trust anchor DER: {e}"
                );
            }
        }
    }

    let mut builder = tls_stream::Builder::new();
    builder
        .cert_store(roots)
        .domain(host)
        .use_sni(true)
        .accept_invalid_hostnames(cfg.skip_verify);
    if cfg.skip_verify {
        builder.verify_callback(|_| Ok(()));
    }

    // `HandshakeError`'s own Display is the generic "failed to perform
    // handshake"; the certificate error a caller needs is in its source. JSSE
    // callers read this message (it is the analogue of HotSpot's "PKIX path
    // building failed: ..."), so unwrap it rather than reporting the wrapper.
    let stream = builder.connect(cred, tcp).map_err(|e| {
        TlsConnectFailure::Handshake(match e {
            schannel::tls_stream::HandshakeError::Failure(io) => {
                format!("TLS handshake failed: {io}")
            }
            other => format!("TLS handshake failed: {other}"),
        })
    })?;

    let peer_cert_chain_der = schannel_peer_chain_der(&stream);
    let negotiated_alpn = stream
        .negotiated_application_protocol()
        .ok()
        .flatten()
        .map(|p| String::from_utf8_lossy(&p).into_owned());
    let raw = stream.get_ref().try_clone().ok();
    let entry = TlsEntry {
        stream: Arc::new(cratonvm_types::lock_order::OrderedPlMutex::new(
            TlsClientStream::Schannel(stream),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )),
        raw,
        peer_host: host.to_string(),
        peer_port: port,
        // SChannel exposes no negotiated-version or ciphersuite accessor
        // through this crate, so these keep the values the native-tls path
        // reported -- unchanged, not newly approximate. The OpenSSL bridge
        // reports the real ones because OpenSSL can be asked.
        negotiated_protocol: String::from("TLSv1.3"),
        negotiated_cipher: String::from("TLS_AES_128_GCM_SHA256"),
        negotiated_alpn,
        peer_cert_chain_der,
    };
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.tls_streams.insert(id, entry);
    Ok(id)
}

/// Client SSLSockets backed by the rustls client path (`t27_tls`) store their
/// stream id offset by this base, so `s2_tls_read`/`write`/`close` can route to
/// the rustls stream table instead of the native-tls one. Native-tls and rustls
/// ids are small counters, so the high offset never collides.
pub const RUSTLS_SOCK_ID_BASE: i32 = 0x4000_0000;

/// A socket returned by `SSLSocketFactory.createSocket(Socket, String, int,
/// boolean)` before its CLIENT-vs-SERVER handshake role is known (real JDK
/// contract: defaults to client mode, but the caller may still call
/// `setUseClientMode(false)` before the handshake actually starts — exactly
/// what MockWebServer's HTTPS listener does) stores a pending-handshake id
/// offset by this base instead of a real rustls stream id. Distinct from,
/// and numerically below, `RUSTLS_SOCK_ID_BASE` so the two ranges never
/// collide; see `t27_tls::{stash_pending_layered_socket, drive_pending_layered_handshake}`.
pub const PENDING_LAYERED_SOCK_ID_BASE: i32 = 0x2000_0000;

/// Third TLS socket-id range: a socket that `SSLSocket.connect(SocketAddress)`
/// has TCP-connected but NOT yet handshaked.
///
/// JSSE splits those two steps -- `Socket.connect` establishes the TCP
/// connection and nothing more; the TLS handshake runs on the first read or
/// write, or on an explicit `startHandshake()`. CratonVM used to do both
/// inside `connect`, which broke every caller that only wants to know whether
/// the port answers: H2 `TcpServer.isRunning()` opens a loopback socket and
/// closes it again WITHOUT any I/O, so on HotSpot it reports "up" while here
/// it inherited the whole handshake, including its failures and its timeouts
/// (measured: 60.8 s and then `TLS handshake failed: the handshake process was
/// interrupted`). `TestTools.testSSL` therefore saw `Server.start()` throw
/// EXCEPTION_OPENING_PORT_2 -- the suite's `Expected: 0 actual: 1`.
///
/// Distinct from, and numerically below, `PENDING_LAYERED_SOCK_ID_BASE`, so
/// the three ranges never collide and a range test can tell them apart. The
/// deferred handshake for this range is driven from exactly the four JSSE
/// trigger points the layered range already uses.
pub const PENDING_CONNECT_SOCK_ID_BASE: i32 = 0x1000_0000;

// ---------------------------------------------------------------------------
// Plaintext readahead for TLS streams
// ---------------------------------------------------------------------------
//
// FIX (tls-handshake-enforcement-gap, doc 21 — `TestSsl` class-level hang).
// `SSLSocketInputStream.read()I` (phases_late/ssl_security.rs) is a native
// call that reads exactly ONE byte, bracketed by
// `begin_blocking_region`/`end_blocking_region` and, for native-tls ids, a
// registry-lock lookup. Real JSSE's `SSLSocketInputStream` serves single-byte
// reads out of a plain Java `byte[]`, so a caller that reads a large body one
// byte at a time (`TestSsl.testPost` — 8 threads x 16 MiB, i.e. ~134 MILLION
// calls) costs HotSpot a few seconds and cost CratonVM ~7.4 minutes, which is
// most of why that whole class timed out.
//
// The buffer is keyed by STREAM ID rather than by the Java `InputStream`
// object: `SSLSocket.getInputStream()` mints a fresh synthetic stream object
// on every call, and unrelated code reads the same id straight through
// `s2_tls_read`, so a per-object buffer could strand already-read bytes.
// Keying by id and draining at the top of `s2_tls_read` keeps every reader of
// a stream consistent no matter which entry point it uses.
//
// Semantics are unchanged: a refill does exactly ONE underlying `read`, which
// returns as soon as any plaintext is available, so this never blocks waiting
// to "fill" the buffer — identical to wrapping the stream in a
// `BufferedInputStream`, which is effectively what the real JDK path is.
pub const TLS_READAHEAD_CAP: usize = 32 * 1024;

pub struct TlsReadahead {
    pub buf: Vec<u8>,
    pub pos: usize,
}

// MEASURED AND REVERTED (testssl-testpost bulk TLS, 2026-08-04): sharding this
// table 64 ways by stream id, on the theory that 8 threads popping ~16.7 million
// single bytes each were convoying on one process-global mutex. They are not.
// A/B on the same probe (`TlsPostShapeProbe.nativeFloor`, which prices
// `available()` — this table's lookup plus one field read — against a no-op
// native on the same receiver):
//
//   global mutex   1 thread 167.5 ns   8 threads 1165.5 ns
//   64 shards      1 thread 180.7 ns   8 threads 1111.7 ns
//
// 4.6% at 8 threads is inside the run-to-run noise, so the lock was never the
// contended resource and the shards bought nothing. The real cost behind that
// number is the per-call field read — see `resolve_field_descriptor_byte_cached`.
pub fn tls_readahead() -> &'static parking_lot::Mutex<HashMap<i32, TlsReadahead>> {
    static T: OnceLock<parking_lot::Mutex<HashMap<i32, TlsReadahead>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Move up to `out.len()` already-buffered plaintext bytes for `id` into
/// `out`. Returns 0 when nothing is buffered (the caller must then do a real,
/// blocking read).
pub fn tls_readahead_drain(id: i32, out: &mut [u8]) -> usize {
    let mut table = tls_readahead().lock();
    let Some(entry) = table.get_mut(&id) else {
        return 0;
    };
    let avail = entry.buf.len() - entry.pos;
    if avail == 0 {
        table.remove(&id);
        return 0;
    }
    let n = avail.min(out.len());
    out[..n].copy_from_slice(&entry.buf[entry.pos..entry.pos + n]);
    entry.pos += n;
    if entry.pos == entry.buf.len() {
        table.remove(&id);
    }
    n
}

/// Number of plaintext bytes currently buffered for `id` — `available()` must
/// count these, since they have already been taken off the socket.
pub fn s2_tls_buffered_len(id: i32) -> usize {
    tls_readahead()
        .lock()
        .get(&id)
        .map(|e| e.buf.len() - e.pos)
        .unwrap_or(0)
}

/// Pop one buffered plaintext byte without any blocking-region bookkeeping.
/// `None` means "nothing buffered" — the caller must fall back to
/// [`s2_tls_fill_readahead`] inside a blocking region.
pub fn s2_tls_pop_buffered_byte(id: i32) -> Option<u8> {
    let mut out = [0u8; 1];
    (tls_readahead_drain(id, &mut out) == 1).then_some(out[0])
}

/// Do ONE real read into the readahead buffer for `id`. Returns the number of
/// bytes buffered (0 = EOF). MUST be called inside a blocking region — it
/// performs genuine blocking socket I/O.
pub fn s2_tls_fill_readahead(id: i32) -> std::io::Result<usize> {
    let mut buf = vec![0u8; TLS_READAHEAD_CAP];
    let n = s2_tls_read_direct(id, &mut buf)?;
    if n == 0 {
        return Ok(0);
    }
    buf.truncate(n);
    tls_readahead()
        .lock()
        .insert(id, TlsReadahead { buf, pos: 0 });
    Ok(n)
}

/// Drop any readahead for `id` — called when the stream is closed so a
/// recycled id can never inherit a dead stream's bytes.
pub fn s2_tls_discard_readahead(id: i32) {
    tls_readahead().lock().remove(&id);
}

/// NEW-13: read from a TLS stream registered via `s2_tls_connect` (native-tls),
/// or — for ids ≥ `RUSTLS_SOCK_ID_BASE` — the rustls client/server stream table.
///
/// Serves any readahead buffered by [`s2_tls_fill_readahead`] first so every
/// reader of a stream observes the same byte sequence regardless of entry
/// point.
pub fn s2_tls_read(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    if !buf.is_empty() {
        let n = tls_readahead_drain(id, buf);
        if n > 0 {
            return Ok(n);
        }
    }
    s2_tls_read_direct(id, buf)
}

/// The unbuffered read — bypasses the readahead entirely. Only
/// [`s2_tls_read`] (after draining) and [`s2_tls_fill_readahead`] may call it.
pub fn s2_tls_read_direct(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    if id >= RUSTLS_SOCK_ID_BASE {
        return crate::t27_tls::rustls_stream_read(id - RUSTLS_SOCK_ID_BASE, buf);
    }
    // LOCK DISCIPLINE: resolve the id and clone the per-stream handle under
    // `s2_registry()`, then RELEASE it before blocking. This mutex guards
    // every synthetic socket/listener/datagram in the process; holding it for
    // the duration of a read that waits on a peer stalls all of them — and a
    // thread parked on a plain mutex inside a native call never reaches a
    // safepoint, so a concurrent STW then waits for it forever. Same
    // reasoning as `s2_blocking_accept`'s existing "clone the handle out
    // under a SHORT lock" comment.
    let (stream, raw) = {
        let reg = s2_registry().lock();
        match reg.tls_streams.get(&id) {
            Some(entry) => (
                entry.stream.clone(),
                entry.raw.as_ref().and_then(|r| r.try_clone().ok()),
            ),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such TLS stream id",
                ));
            }
        }
    };
    #[cfg(unix)]
    if let Some(ref raw_sock) = raw {
        use std::os::unix::io::AsRawFd;
        let has_buffered = {
            let mut guard = stream.lock();
            match &mut *guard {
                TlsClientStream::Openssl(s) => s.ssl().pending() > 0,
                TlsClientStream::Native(_) => false,
            }
        };
        if !has_buffered {
            let fd = raw_sock.as_raw_fd();
            let timeout_ms = raw_sock
                .read_timeout()
                .ok()
                .flatten()
                .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
                .unwrap_or(-1);
            let deadline = if timeout_ms >= 0 {
                Some(
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(timeout_ms as u64),
                )
            } else {
                None
            };
            loop {
                if !s2_registry().lock().tls_streams.contains_key(&id) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "socket closed",
                    ));
                }
                let slice_ms = match deadline {
                    Some(dl) => {
                        let rem = dl.saturating_duration_since(std::time::Instant::now());
                        if rem.is_zero() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "timed out",
                            ));
                        }
                        rem.as_millis().min(200) as i32
                    }
                    None => 200,
                };
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pfd, 1, slice_ms) };
                if rc > 0 {
                    break;
                }
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
            }
        }
    }
    let mut guard = stream.lock();
    let result = match &mut *guard {
        TlsClientStream::Native(stream) => stream.read(buf),
        #[cfg(unix)]
        TlsClientStream::Openssl(stream) => stream.read(buf),
        #[cfg(windows)]
        TlsClientStream::Schannel(stream) => stream.read(buf),
    };
    drop(guard);
    s2_tls_classify_after_block(id, result)
}

/// Re-ask the registry AFTER a blocking TLS call has returned, and report a
/// concurrent `close()` as such instead of as EOF or as a peer error.
///
/// This is the close-awareness half of W7-53's mechanism that a TLS record
/// layer CAN safely take. The other half — parking in `poll` on a bounded
/// slice and abandoning the wait when the registry entry disappears — must NOT
/// be transplanted here: `native_tls::TlsStream::read` assembles a TLS record
/// across an unbounded number of underlying `recv` calls and exposes no
/// "is a whole record available" query, so a loop that returned between two of
/// them would hand the caller a partial record and desynchronise the stream
/// for good. Classifying a call that has ALREADY returned cannot do that: the
/// record layer is at rest at that point, by construction.
///
/// `ErrorKind::Interrupted` is the carrier the rest of this family uses
/// (`net_phase_e::re1_socket_closed_err`, `socket_channel::
/// channel_async_closed_err`) and it is unambiguous here for the same reason:
/// the only producer below is this function, and it produces it only when the
/// id has left the registry — a state no successful I/O can be in.
///
/// What this does NOT do on its own is END the wait. That is
/// [`s2_tls_close`]'s job (it shuts the duplicate handle down), and on Windows
/// it still cannot: see the note there.
pub fn s2_tls_classify_after_block(id: i32, result: std::io::Result<usize>) -> std::io::Result<usize> {
    // Cheap and exact: a live id is still in the table. Taken AFTER the call,
    // deliberately — a close that landed while this thread was parked is then
    // observed on the very next instruction, and a close that raced a
    // readiness edge still wins, which is what HotSpot does (it fails an I/O a
    // concurrent `close()` beat rather than handing back bytes on a socket
    // Java has already closed).
    if s2_registry().lock().tls_streams.contains_key(&id) {
        return result;
    }
    match result {
        // Bytes that genuinely arrived before the close are still delivered:
        // dropping them would lose data the peer really sent, and the NEXT
        // call reports the close.
        Ok(n) if n > 0 => Ok(n),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "socket closed",
        )),
    }
}

/// NEW-13: write to a TLS stream registered via `s2_tls_connect` (native-tls),
/// or — for ids ≥ `RUSTLS_SOCK_ID_BASE` — the rustls stream table.
pub fn s2_tls_write(id: i32, data: &[u8]) -> std::io::Result<usize> {
    if id >= RUSTLS_SOCK_ID_BASE {
        return crate::t27_tls::rustls_stream_write(id - RUSTLS_SOCK_ID_BASE, data);
    }
    // Same lock discipline as `s2_tls_read` — see its doc comment.
    let stream = {
        let reg = s2_registry().lock();
        match reg.tls_streams.get(&id) {
            Some(entry) => entry.stream.clone(),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such TLS stream id",
                ));
            }
        }
    };
    let mut guard = stream.lock();
    let result = match &mut *guard {
        TlsClientStream::Native(stream) => stream.write(data),
        #[cfg(unix)]
        TlsClientStream::Openssl(stream) => stream.write(data),
        #[cfg(windows)]
        TlsClientStream::Schannel(stream) => stream.write(data),
    };
    drop(guard);
    // Same after-the-fact classification as the read side, and safe for the
    // same reason — see `s2_tls_classify_after_block`. A partial write that
    // did land is reported as such; the next call reports the close.
    s2_tls_classify_after_block(id, result)
}

/// NEW-13: perform a graceful TLS shutdown (close_notify) and drop the stream.
/// Idempotent: closing an unknown id is a no-op.
pub fn s2_tls_close(id: i32) -> std::io::Result<()> {
    s2_tls_discard_readahead(id);
    if id >= RUSTLS_SOCK_ID_BASE {
        crate::t27_tls::rustls_stream_close(id - RUSTLS_SOCK_ID_BASE);
        return Ok(());
    }
    // Unregister under the registry lock, shut down outside it.
    let entry = s2_registry().lock().tls_streams.remove(&id);
    if let Some(entry) = entry {
        // Graceful shutdown first when the stream is idle; never wait behind
        // a blocked reader. The descriptor shutdown below guarantees wakeup.
        if let Some(raw) = entry.raw.as_ref() {
            let _ = raw.set_write_timeout(Some(std::time::Duration::from_millis(200)));
            let _ = raw.shutdown(std::net::Shutdown::Read);
        }
        if let Some(mut stream) = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            let mut guard = entry.stream.try_lock();
            while guard.is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
                guard = entry.stream.try_lock();
            }
            guard
        } {
            match &mut *stream {
                TlsClientStream::Native(stream) => {
                    let _ = stream.shutdown();
                }
                #[cfg(unix)]
                TlsClientStream::Openssl(stream) => {
                    let _ = stream.shutdown();
                }
                #[cfg(windows)]
                TlsClientStream::Schannel(stream) => {
                    let _ = stream.shutdown();
                }
            }
        }
        if let Some(raw) = entry.raw.as_ref() {
            let _ = raw.shutdown(std::net::Shutdown::Both);
        }
    }
    Ok(())
}

/// NEW-13: snapshot the captured peer certificate chain (DER bytes) for a
/// given TLS stream id, or -- for ids >= `RUSTLS_SOCK_ID_BASE` -- the rustls
/// client stream table. Returns an empty Vec if no cert was presented.
///
/// FIX (tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals):
/// this was missing the same `RUSTLS_SOCK_ID_BASE` redirect `s2_tls_read`/
/// `s2_tls_write`/`s2_tls_close` (just above) already have. A client TLS
/// socket backed by the rustls path (`t27_tls`, e.g. one that went through
/// the deferred `SSLSocketFactory.createSocket(Socket,...)` handshake) stores
/// its stream id offset by `RUSTLS_SOCK_ID_BASE` -- looking that id up in
/// `s2_registry()` (the native-tls-only table) always misses, so
/// `SSLSession.getPeerCertificates()`/`getPeerPrincipal()` (both call this)
/// silently saw an empty chain and reported a "peer not authenticated" error
/// even though rustls had genuinely captured the peer certificate, and
/// `t27_tls::rustls_client_peer_cert_chain_der` already exposed it correctly
/// for a different call site (`record_client_peer_chain`'s caller).
pub fn s2_tls_peer_cert_chain_der(id: i32) -> Option<Vec<Vec<u8>>> {
    if id >= RUSTLS_SOCK_ID_BASE {
        return crate::t27_tls::rustls_client_peer_cert_chain_der(id - RUSTLS_SOCK_ID_BASE);
    }
    let reg = s2_registry().lock();
    reg.tls_streams
        .get(&id)
        .map(|e| e.peer_cert_chain_der.clone())
}

/// Why this stream's TLS handshake failed, for a stream that was registered
/// in a failed state instead of having the failure thrown at its creator —
/// see `t27_tls::TlsServerStream::HandshakeFailed`. `None` for every healthy
/// stream, and for every client-side one (a client handshake failure is
/// reported by `SSLSocketFactory.createSocket` / `SSLSocket.connect` itself,
/// which is where JSSE reports it).
pub fn s2_tls_handshake_failure(id: i32) -> Option<String> {
    if id >= RUSTLS_SOCK_ID_BASE {
        return crate::t27_tls::rustls_server_handshake_failure(id - RUSTLS_SOCK_ID_BASE);
    }
    None
}

/// NEW-13 / T2.7.11: lookup the negotiated (protocol, cipher, host, port)
/// tuple for a TLS stream id. Used to populate SSLSession fields lazily.
pub fn s2_tls_session_info(id: i32) -> Option<(String, String, String, u16)> {
    let reg = s2_registry().lock();
    reg.tls_streams.get(&id).map(|e| {
        (
            e.negotiated_protocol.clone(),
            e.negotiated_cipher.clone(),
            e.peer_host.clone(),
            e.peer_port,
        )
    })
}

/// T2.7.11: lookup the ALPN protocol negotiated for a given TLS stream,
/// returning `None` if ALPN was not negotiated (or the stream id is unknown).
pub fn s2_tls_negotiated_alpn(id: i32) -> Option<String> {
    let reg = s2_registry().lock();
    reg.tls_streams
        .get(&id)
        .and_then(|e| e.negotiated_alpn.clone())
}

// ---- Field index constants -------------------------------------------------

pub const BB_ARRAY: usize = 0;
pub const BB_POS: usize = 1;
pub const BB_LIMIT: usize = 2;
pub const BB_CAP: usize = 3;
pub const BB_MARK: usize = 4;
pub const BB_ORDER: usize = 5; // 0=BIG_ENDIAN, 1=LITTLE_ENDIAN
                           // Real `Buffer.segment` field index (`final java.lang.foreign.MemorySegment
                           // segment`) — the only Object-typed slot among a real-JDK-shaped typed
                           // NIO buffer view's 6 physical fields (mark/position/limit/capacity/
                           // address/segment). Used to stash the backing array reference for
                           // IntBuffer/LongBuffer/ShortBuffer/FloatBuffer/DoubleBuffer views, whose
                           // abstract class declares no `hb` field to write by name — see
                           // `s2_bb_arr`'s fallback and `s2_bb_synthetic_layout`'s class-name guard
                           // (both fixed together 2026-07-11; writing here without that guard gets
                           // silently clobbered by `s2_bb_set_order`, which used to also treat slot 5
                           // as an int order flag for these same 6-field objects).
pub const BB_SEGMENT_SLOT: usize = 5;
// NEW-17: extra fields for direct buffers. Bytes 6..7 are only populated
// by `allocateDirect`; non-direct buffers leave them at default (0).
pub const BB_NATIVE_ID: usize = 6; // Long  — alloc_id from NativeMemoryTable, 0 if heap
pub const BB_DIRECT_FLAG: usize = 7; // Int   — 1 if direct, 0 otherwise

// jdk/internal/ref/Cleaner$Deallocator synthetic for DirectByteBuffer.
//   field 0 = alloc_id (Long)          — key into the VM's `NativeMemoryTable`
//   field 1 = global-root handle (Long) — the root that keeps the owning
//             `Cleaner$Cleanable` reachable until the action has run; see
//             `s2_bb_alloc_direct`. Zeroed by `run()` once released.
pub const DEALLOC_ID: usize = 0;
pub const DEALLOC_ROOT: usize = 1;
pub const DEALLOC_FIELDS: usize = 2;
pub const DEALLOC_CLASS: &str = "jdk/internal/ref/DirectBufferDeallocator";

/// Alignment of the off-heap block behind a synthetic `DirectByteBuffer`.
/// 8 bytes is sufficient for every primitive `Unsafe.put*` width, matching
/// `native-io/src/direct_buffer.rs`'s `DBB_ALIGN`.
pub const DIRECT_BUFFER_ALIGN: usize = 8;

pub const S2SC_CONNECTED: usize = 0;
pub const S2SC_OPEN: usize = 1;
pub const S2SC_ADDR: usize = 2;
pub const S2SC_SOCK_ID: usize = 3;
pub const S2SC_BLOCKING: usize = 4;

pub const S2SSC_OPEN: usize = 0;
pub const S2SSC_BOUND: usize = 1;
pub const S2SSC_LISTENER_ID: usize = 2;
pub const S2SSC_PORT: usize = 3;
pub const S2SSC_PENDING: usize = 4;

pub const S2SEL_OPEN: usize = 0;
pub const S2SEL_KEYS: usize = 1;
pub const S2SEL_NKEYS: usize = 2;

// NEW-3: DatagramChannel synthetic field indices. The previous 4-field
// layout in `register_datagram_channel` (port/open/connected/blocking)
// did not carry a persistent socket id, so every send/receive re-bound a
// fresh UDP socket. The NEW-3 refactor keeps the original four fields and
// adds a fifth: the registry socket id (-1 when unallocated). Existing
// callers that read only the first four fields continue to work; the
// selector poll path and `send`/`receive`/`read`/`write` now read field
// 4 to locate the persistent socket in `SocketRegistry::dgrams`.
pub const S2DC_PORT: usize = 0;
pub const S2DC_OPEN: usize = 1;
pub const S2DC_CONNECTED: usize = 2;
pub const S2DC_BLOCKING: usize = 3;
pub const S2DC_SOCK_ID: usize = 4;

// ---- ByteBuffer helpers ----------------------------------------------------

/// The class to stamp on a HEAP `ByteBuffer`: the concrete `HeapByteBuffer`
/// (or `HeapByteBufferR` for a read-only view) when its real-JDK bytecode is
/// available, else the abstract `java/nio/ByteBuffer` exactly as before.
///
/// `java.nio.ByteBuffer` is ABSTRACT. Stamping it leaves every method without a
/// CratonVM native dispatching to an abstract declaration, i.e.
/// `AbstractMethodError: ... has no Code attribute` for the first caller of
/// anything the S2 surface does not cover. Nothing hits it today only because
/// `register_essential_natives` happens to give those methods bodies -- the
/// exposure is conditional, not absent. Naming the concrete class gives them
/// real JDK bodies instead, which is what HotSpot reports
/// (`kind=HeapByteBuffer` / `kind=HeapByteBufferR`).
///
/// Both probes are needed and neither alone is enough -- the same idiom
/// `allocateDirect` above uses. `would_fabricate_synthetic_stub` is the
/// non-destructive "are the class bytes reachable" question, but it answers
/// "no stub" once ANY earlier caller has already minted one;
/// `is_class_synthetic_stub` covers exactly that case. Falling back to the
/// abstract name matters: a synthetic stub would trade `AbstractMethodError`
/// for a buffer whose every method is a silent no-op, which is strictly worse
/// than the status quo.
pub fn s2_bb_heap_class(ctx: &mut dyn NativeContext, read_only: bool) -> &'static str {
    let name = if read_only {
        "java/nio/HeapByteBufferR"
    } else {
        "java/nio/HeapByteBuffer"
    };
    if !ctx.would_fabricate_synthetic_stub(name) && !ctx.is_class_synthetic_stub(name) {
        name
    } else {
        "java/nio/ByteBuffer"
    }
}

pub fn s2_bb_alloc(
    ctx: &mut dyn NativeContext,
    cap: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    use cratonvm_types::ArrayElementType;
    // `ByteBuffer.allocate(n)` is caller-sized: `n` comes straight from Java,
    // and on a full heap the backing `new byte[n]` must raise a *catchable*
    // OutOfMemoryError (what HotSpot does) rather than abort the VM. This is
    // the same fallible-allocator idiom as the ArrayList(int)/StringBuilder(int)
    // capacity-constructor family — see
    // `crash-01-arraylist-capacity-oom-abend.md`. Found via
    // H2's `org.h2.test.db.TestOutOfMemory`, whose MVStore-on-memFS workload
    // allocates ~76 MB buffers until the heap is gone.
    //
    // RECLAIM AND RETRY, not one shot (H2 `TestBenchmark`, 2026-08-18). Being
    // *fallible* stopped the abort; it did not make the refusal honest.
    // `try_new_array` deliberately does not collect -- see
    // `runtime::native_oom` -- so this native reported `OutOfMemoryError` on
    // the FIRST refusal, while the two paths that allocate an array from
    // bytecode (`gc_alloc_array`, `jit_newarray`) both run a ladder: retire
    // the TLAB, force a collection, retry, `last_ditch_reclaim`, retry again,
    // and only then throw. `ByteBuffer.allocate` is shadowed by this native in
    // real-JDK mode too, so its backing array never sees that ladder.
    //
    // Measured: MVStore's background writer grows a `WriteBuffer` to
    // 10,616,832 bytes at `-Xmx1g` on ZGC. The arena has no hole that big at
    // that instant (498 KiB largest, 348 MB free across 30k spans) and the
    // request was refused -- with the heap 97% free once the collection nobody
    // asked for finally ran. Repeating the identical `ByteBuffer.allocate` one
    // Java statement later succeeded on the first attempt, and the class also
    // passes under `--nojit`, at `-Xmx2g`, and on the generational collector:
    // a spurious refusal, not an exhausted heap.
    //
    // Calling `reclaim_before_alloc_retry` is legal HERE specifically, and the
    // precondition is the caller's to prove: this is the native's first
    // allocation, so it holds no unpinned `ObjectRef` in a Rust local for a
    // collection to dangle or sweep. Note that the very next allocation below
    // must NOT do this -- `arr` is live by then, which is exactly why it is
    // pinned across it.
    let arr = match ctx.try_new_array(ArrayElementType::Byte, cap) {
        Some(a) => a,
        None => {
            let reclaimed = ctx.reclaim_before_alloc_retry();
            let served = reclaimed
                .then(|| ctx.try_new_array(ArrayElementType::Byte, cap))
                .flatten();
            // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
            let served = served.or_else(|| {
                (reclaimed && ctx.reclaim_after_failed_alloc_retry())
                    .then(|| ctx.try_new_array(ArrayElementType::Byte, cap))
                    .flatten()
            });
            match served {
                Some(a) => a,
                None => return Ok(None),
            }
        }
    };
    // GC-safety: the synthetic allocation below can collect (class
    // initialisation runs Java, and the reclaiming funnel collects on a
    // refusal), which relocates `arr` (read again by `bb_write_hb`
    // immediately after); pin it and re-read.
    //
    // gc-common w8-b: the buffer object takes the RECLAIMING funnel. It is
    // legal here because `arr` is pinned and is the only reference this
    // native holds (the caller, the `allocate` closure, holds none; `cls`
    // is a `&str`). Before, a refusal of these few slots threw
    // `OutOfMemoryError` with the backing array already allocated and no
    // collection asked for (`common-w3c-native-callback-allocations-cannot-collect`).
    let arr_pin = ctx.pin_native_root(arr);
    let cls = s2_bb_heap_class(ctx, false);
    let buf = crate::try_alloc_concurrent_synthetic_reclaiming(ctx, cls, 6)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    bb_write_hb(ctx, buf, arr, cap as i32);
    Ok(Some(buf))
}

/// NEW-17 — synthetic-mode `ByteBuffer.allocateDirect(cap)`.
///
/// The result is a genuinely DIRECT buffer: `cap` bytes of real off-heap
/// memory from the VM's `NativeMemoryTable`, released when the buffer becomes
/// unreachable. It reuses the six-slot synthetic ByteBuffer shape every
/// `s2_bb_*` accessor in this file already understands, with one difference
/// from the heap flavour built by `s2_bb_alloc`: `BB_ARRAY` stays null and
/// `BB_MARK` (slot 4) carries the native address — exactly what
/// `s2_bb_direct_addr` probes for, and the same convention the direct
/// typed-buffer views produced by `s2_view_buf_fn!` already use. So
/// `isDirect()` answers true, `hasArray()` false, `array()` throws
/// `UnsupportedOperationException`, and every get/put routes through
/// `copy_from_native_memory`/`copy_to_native_memory` against the real block.
/// (The cost of reusing slot 4 is that such a buffer has no `mark` — see the
/// guard in `s2_bb_set_mark`, which drops the write rather than overwriting
/// the backing pointer with a small integer.)
///
/// Reclamation is the NEW-17 Cleaner pipeline, not a finaliser:
/// `discover_reference(Cleaner, cleanable, buf)` registers the Cleanable as a
/// phantom over the buffer, so when the buffer dies the reference processor
/// emits the Cleanable into `cleaner_actions`, `CleanerThread` queues it, and
/// `interpreter::run_cleaner_actions` invokes `run()V` on the deallocator
/// registered under `DEALLOC_CLASS` below. An explicit
/// `Cleaner$Cleanable.clean()` reaches the same `run()` through the shared
/// `cleaned` flag, so the block can never be freed twice.
pub fn s2_bb_alloc_direct(ctx: &mut dyn NativeContext, cap: i32) -> MethodCallResult {
    let allocation = ctx.allocate_native_memory(cap.max(0) as usize, DIRECT_BUFFER_ALIGN);
    let (block_id, ptr) = match allocation {
        Some(block) => block,
        None => {
            return Err(RuntimeError::OutOfMemoryError {
                message: format!("Direct buffer memory: tried {cap}"),
            }
            .into())
        }
    };
    let addr = ptr as usize as i64;

    // GC SAFETY: each allocation below is a collection point. `buf` and
    // `dealloc` are pinned across the later ones and re-read through the pins;
    // `cleanable` is minted last so nothing can move it before its raw address
    // reaches the reference processor.
    let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6)?;
    let buf_pin = ctx.pin_native_root(buf);
    let dealloc = try_alloc_concurrent_synthetic(ctx, DEALLOC_CLASS, DEALLOC_FIELDS)?;
    let dealloc_pin = ctx.pin_native_root(dealloc);
    let cleanable: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "java/lang/ref/Cleaner$Cleanable", CLEANABLE_FIELDS)?;
    let buf = ctx.read_native_pin(buf_pin, buf);
    let dealloc = ctx.read_native_pin(dealloc_pin, dealloc);
    ctx.unpin_native_roots(buf_pin);

    // No heap array: `s2_bb_arr` must answer None so `s2_bb_direct_addr` is
    // consulted and the buffer reads as direct.
    //
    // By NAME first, then the indexed overlay behind the layout screen — the
    // same order and the same guard `bb_write_hb` and `s2_bb_as_char_buffer`
    // already use, and for the same reason (G38-1, closing the last unguarded
    // member of that family). `alloc_concurrent_synthetic` resolves the REAL
    // (abstract) `java.nio.ByteBuffer` whenever its class bytes are reachable,
    // and on that layout the six indices alias
    // `mark@0/position@1/limit@2/capacity@3/address@4/segment@5`: the old
    // unconditional block wrote `Object(None)` over `mark` (an `int`, coerced
    // to 0 where the JDK's own ctor leaves -1) and `Int(0)` over `segment` (a
    // reference, coerced to null — so the ORDER FLAG WAS DESTROYED and every
    // such buffer decoded as BIG_ENDIAN whatever `order(LITTLE_ENDIAN)` set).
    // Both are the species in
    // G30-1-the-silent-reference-slot-coercion-20260817.md, in both
    // directions. The by-name writes put each value in the field that actually
    // holds it — `address` is where `s2_bb_direct_addr` looks FIRST, and
    // `seed_buffer_byte_order` writes the `bigEndian`/`nativeByteOrder` pair
    // `s2_bb_order` reads on a real layout — so the direct-buffer convention
    // is unchanged on both shapes.
    //
    // `hb`, `offset` and `isReadOnly` are deliberately NOT written: a fresh
    // allocation already reads back null/0 for all three, which is exactly the
    // direct, zero-offset, writable shape, and every name written here is one
    // more slot to get wrong on a layout this function cannot see.
    ctx.set_field_by_name(buf, "position", Value::Int(0));
    ctx.set_field_by_name(buf, "limit", Value::Int(cap));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    ctx.set_field_by_name(buf, "address", Value::Long(addr));
    cratonvm_native_io::seed_buffer_byte_order(ctx, buf);
    if s2_bb_indexed_layout(ctx, buf) {
        ctx.set_field(buf, BB_ARRAY, Value::Object(None));
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(cap));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Long(addr));
        ctx.set_field(buf, BB_ORDER, Value::Int(0)); // JDK default: BIG_ENDIAN
    }

    ctx.set_field(dealloc, DEALLOC_ID, Value::Long(block_id));
    ctx.set_field(cleanable, CLEANABLE_ACTION, Value::Object(Some(dealloc)));
    ctx.set_field(cleanable, CLEANABLE_CLEANED, Value::Int(0));
    ctx.set_field(cleanable, CLEANABLE_INDEX, Value::Int(-1));

    // The ReferenceProcessor keeps only the Cleanable's raw ADDRESS and is not
    // a GC root, so without a strong reference the Cleanable would be
    // collected alongside the buffer and the block would leak. There is no
    // owning `Cleaner` object on this path to park it in (the JDK's
    // `DirectByteBuffer` uses the static `CleanerFactory` list), so take a
    // persistent global root instead — remapped by the moving collector — and
    // record its handle in the deallocator, which drops it in `run()`.
    let root = ctx.add_global_root(cleanable);
    ctx.set_field(dealloc, DEALLOC_ROOT, Value::Long(root as i64));

    ctx.discover_reference(REF_TYPE_CLEANER, cleanable, buf, None);
    Ok(Some(Value::Object(Some(buf))))
}

/// Initialise a synthetic ByteBuffer so BOTH the indexed-slot layout
/// (BB_ARRAY/BB_POS/...) AND the real-JDK named fields (`hb`, `offset`,
/// `position`, `limit`, `capacity`, `mark`) point at the same byte[].
/// Without the by-name writes, JDK bytecode that reads `hb` directly
/// (e.g. `ByteBuffer.hasArray`, `ByteBuffer.array`) sees null because
/// the indexed slot 0 landed on Buffer.mark (int descriptor → Object
/// coerced to Int by descriptor-aware set_field).
///
/// BUG (found 2026-07-12): on a real-JDK-shaped object the indexed
/// fallback below does not land on a harmless/unused slot 0 — the real
/// `Buffer` layout is `mark@0/position@1/limit@2/capacity@3/address@4/
/// segment@5`, so `BB_MARK` (index 4) aliases `address` and `BB_ORDER`
/// (index 5) aliases `segment`. Since this fallback runs AFTER the
/// correct by-name writes, `set_field(buf, BB_MARK, Int(-1))` clobbered
/// the real `address` field (needed by every bulk `get`/`put` via
/// `ScopedMemoryAccess.copyMemory`) with -1, and `set_field(buf,
/// BB_ARRAY, Object(arr))` clobbered real `mark` with a coerced,
/// truncated array-pointer int. `ByteBuffer.allocate(n)` then threw
/// `ArrayIndexOutOfBoundsException` on the very first bulk put/get (see
/// zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md).
/// Reuse the same `s2_bb_synthetic_layout` discriminator the
/// 2026-07-11 typed-buffer-view fix uses for the identical slot-5/
/// segment collision: only apply the indexed fallback when the object
/// is genuinely the bare 6-slot synthetic layout, not a real-JDK class
/// whose by-name writes above already did the job.
pub fn bb_write_hb(ctx: &mut dyn NativeContext, buf: ObjectRef, arr: ObjectRef, cap: i32) {
    ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(buf, "offset", Value::Int(0));
    ctx.set_field_by_name(buf, "isReadOnly", Value::Int(0));
    ctx.set_field_by_name(buf, "position", Value::Int(0));
    ctx.set_field_by_name(buf, "limit", Value::Int(cap));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    // Mirror the JDK field initializer: fresh buffers default to
    // BIG_ENDIAN. In real-JDK mode `alloc_concurrent_synthetic` resolves
    // the REAL (abstract) `java.nio.ByteBuffer` class and allocates its
    // full field layout, so without this seed the named `bigEndian` slot
    // reads back Int(0) — which `s2_bb_order` would decode as
    // LITTLE_ENDIAN by default.
    //
    // CONVERGED (F26-1, closing the two-of-five sites of
    // W7-76-bytebuffer-alias-residuals.md §8.2 that this lane owns). This was
    // one of five byte-identical transcriptions of the same two writes, with
    // "nothing to make them agree"; it is now a delegation to
    // `native_io::seed_buffer_byte_order`, for the same reason and by the same
    // crate-direction argument as `s2_bb_is_read_only` above. The remaining
    // three sites are F26-1 §6 nominations.
    cratonvm_native_io::seed_buffer_byte_order(ctx, buf);
    // Real HeapByteBuffer seeds Buffer.address to ARRAY_BYTE_BASE_OFFSET +
    // offset (16 for a fresh, zero-offset heap buffer). Bulk get/put
    // bytecode routes through ScopedMemoryAccess and expects this
    // base-offset-relative value when copying from/to hb. `bb_write_hb` writes
    // `offset = 0` four lines up, so the offset term is 0 — spelled through
    // this file's `ARRAY_BYTE_BASE_OFFSET`, which is now DEFINED AS
    // native-io's, so the two crates cannot disagree about the value.
    ctx.set_field_by_name(buf, "address", Value::Long(ARRAY_BYTE_BASE_OFFSET));
    // Synthetic-mode indexed fallback — ONLY for the bare synthetic layout
    // (no real Buffer/ByteBuffer field metadata). On a real-JDK-shaped
    // object these indices alias real fields (mark@0, address@4,
    // segment@5) that the by-name writes above already set correctly;
    // redoing them here would clobber address/mark as described above.
    // (And a `--synthetic-jdk` stub, whose by-name writes above are all
    // no-ops: `s2_bb_indexed_layout`, interpreter round i5 wave 105.)
    if s2_bb_indexed_layout(ctx, buf) {
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(cap));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Int(-1));
        ctx.set_field(buf, BB_ORDER, Value::Int(0));
    }
}

#[inline]
pub fn s2_bb_pos(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_POS).as_int().unwrap_or(0)
}
#[inline]
pub fn s2_bb_limit(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_LIMIT).as_int().unwrap_or(0)
}
/// `Buffer.checkIndex` for the ABSOLUTE accessors: `index` must admit `width`
/// bytes within the buffer's **limit**.
///
/// This did not exist until 2026-08-05 and none of the twelve absolute
/// accessors below bounds-checked at all. `ByteBuffer.wrap(new byte[8]).get(99)`
/// returned **0** where HotSpot throws; `getInt(6)` on the same buffer returned
/// `117964800`, i.e. the two real bytes at 6 and 7 followed by two fabricated
/// zeroes. Silently wrong DATA is worse than an exception, because a
/// computation consumes it.
///
/// `s2_bb_get_byte` already refuses to read out of range -- it returns a benign
/// 0 and says so in its comment -- but it returns `i8` and so has no way to
/// raise a Java exception. That defensiveness is the right thing for a helper
/// and the wrong thing for the API contract, so the check belongs HERE, at the
/// registration sites, where `MethodCallResult` can carry the throw. The
/// helper's zero stays as the panic guard it was written to be.
///
/// **`limit`, not `capacity`** -- `ByteBuffer.get(int)` is `Objects.checkIndex(i,
/// limit)`. A buffer whose limit has been pulled in must refuse an absolute read
/// past it even though the storage is still there.
///
/// HotSpot's message here is null (`Buffer.checkIndex` throws the no-arg
/// `IndexOutOfBoundsException`), which is why this passes `None` rather than
/// inventing text -- checked against a HotSpot 25 control, not assumed.
pub fn s2_bb_check_index(
    ctx: &dyn NativeContext,
    buf: ObjectRef,
    index: i32,
    width: i32,
) -> Result<(), MethodCallFailed> {
    let limit = s2_bb_limit(ctx, buf);
    // Widened so `index + width` cannot overflow for an index near i32::MAX.
    let end = index as i64 + width as i64;
    if index < 0 || end > limit as i64 {
        return Err(
            cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { message: None }.into(),
        );
    }
    Ok(())
}

#[inline]
pub fn s2_bb_cap(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_CAP).as_int().unwrap_or(0)
}

/// `Objects.checkFromIndexSize(from, size, length)` for the buffer natives
/// that shadow the bytecode which would otherwise call it —
/// `slice(index, length)` and the bulk `get`/`put(byte[], off, len)` array-side
/// check.
///
/// Unlike [`s2_bb_check_index`], these DO carry a detail message: the real
/// callers reach `Preconditions` through `Objects`, whose `null` formatter
/// makes `outOfBoundsMessage` text part of the exception.
///
/// The class is `IndexOutOfBoundsException` exactly — not the
/// `ArrayIndexOutOfBoundsException` these sites used to raise, which was filed
/// at the time as benign because `catch (IndexOutOfBoundsException)` still
/// matched. It is not benign: the SUBCLASS direction is the one that breaks a
/// `catch`, so `catch (ArrayIndexOutOfBoundsException)` around one of these
/// calls matched here and missed on a real JVM — and while it stood, a
/// type-exact differential could never agree with HotSpot, so it could detect
/// nothing new either.
pub fn s2_check_from_index_size(from: i32, size: i32, length: i32) -> Result<(), MethodCallFailed> {
    let bad =
        from < 0 || size < 0 || length < 0 || i64::from(from) + i64::from(size) > i64::from(length);
    if !bad {
        return Ok(());
    }
    Err(
        RuntimeError::ioobe(crate::preconditions::CheckKind::FromIndexSize.message(&[
            i64::from(from),
            i64::from(size),
            i64::from(length),
        ]))
        .into(),
    )
}

/// Read a ByteBuffer's `mark`, preferring the real-JDK named field.
///
/// `Buffer`'s actual real-JDK field order is `mark(0), position(1),
/// limit(2), capacity(3), address(4)` (see `t27_tls.rs`'s `bb_view` doc
/// comment / the FIXED bug it documents for the same class of issue). The
/// indexed `BB_MARK = 4` constant used throughout this file for the
/// synthetic-mode layout (`array, pos, limit, cap, mark, order`) therefore
/// lands on real field 4 — `address` (a `long`) — for real-JDK-mode
/// `ByteBuffer` objects, NOT `mark` (real field 0). `position`/`limit`/
/// `capacity` happen to align (real indices 1/2/3 match `BB_POS`/`BB_LIMIT`/
/// `BB_CAP`), which is what let this go unnoticed: only `mark`/`reset` were
/// silently broken (every `reset()` on a real ByteBuffer threw
/// `InvalidMarkException`, even immediately after a matching `mark()` —
/// found via Tomcat's `TestHttp2Limits`, whose HTTP/2 header-block-fragment
/// buffering hits this mark/reset idiom on every request). Falls back to
/// the indexed slot for genuinely synthetic (non-real-JDK) buffer objects
/// that have no `mark` field to resolve by name.
#[inline]
pub fn s2_bb_get_mark(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    match ctx.get_field_by_name(buf, "mark") {
        Value::Int(m) => m,
        _ => ctx.get_field(buf, BB_MARK).as_int().unwrap_or(-1),
    }
}

/// Write a ByteBuffer's `mark` to both the real-JDK named field and the
/// synthetic indexed slot. See `s2_bb_get_mark` for why the named field is
/// authoritative for real-JDK-mode objects.
#[inline]
pub fn s2_bb_set_mark(ctx: &mut dyn NativeContext, buf: ObjectRef, value: i32) {
    // Only fall back to the indexed synthetic slot when there is truly no
    // real `mark` field to resolve by name (a genuinely synthetic buffer
    // class, e.g. when the real JDK class failed to load). For real-JDK
    // ByteBuffer/DirectByteBuffer objects, index 4 aliases the real
    // `address` field (`Buffer{mark,position,limit,capacity,address}`) —
    // for `DirectByteBuffer` specifically, `address` is the actual native
    // memory pointer backing the buffer, so writing our `mark` value there
    // unconditionally corrupts it, crashing the very next `get`/`put` with
    // a wild-pointer SIGSEGV (found via a `ByteBuffer.allocateDirect` +
    // `mark()`/`put()` repro while fixing the heap-buffer InvalidMarkException
    // bug above). `get_field_by_name` returns `Value::Object(None)` only when
    // field resolution itself fails (see `vm_exec.rs::get_field_by_name`),
    // which distinguishes "no such field" from "real int field valued 0".
    match ctx.get_field_by_name(buf, "mark") {
        Value::Object(None) => {
            // A SYNTHETIC direct buffer stores its native address in this very
            // slot (see `s2_bb_alloc_direct` and the direct branch of
            // `s2_view_buf_fn!`), so it has no mark slot at all. Writing an Int
            // here would replace the backing pointer with a small integer and
            // turn the next get/put into a wild native access. Drop the mark
            // instead — a later `reset()` then reports "no mark", which is a
            // recoverable `InvalidMarkException` rather than a SIGSEGV. Only
            // reachable when `mark` did NOT resolve by name, so a real-JDK
            // buffer (whose slot 4 is also a positive `address` long) never
            // takes this branch.
            if matches!(ctx.get_field(buf, BB_MARK), Value::Long(addr) if addr > 0) {
                return;
            }
            ctx.set_field(buf, BB_MARK, Value::Int(value))
        }
        _ => ctx.set_field_by_name(buf, "mark", Value::Int(value)),
    }
}
/// True for the 6-slot pure-synthetic ByteBuffer layout that the indexed
/// `BB_*` accessors address. In REAL-JDK mode this is false for every s2
/// `ByteBuffer`: `try_alloc_concurrent_synthetic(_, "java/nio/ByteBuffer", 6)?`
/// resolves the real (abstract) class and allocates its FULL field layout
/// (11 slots — `Buffer{mark,position,limit,capacity,address,segment}` +
/// `ByteBuffer{hb,offset,isReadOnly,bigEndian,nativeByteOrder}`), where
/// slot 5 aliases `segment`, not the synthetic order slot.
///
/// BUG (found 2026-07-11): field COUNT alone is not a reliable
/// discriminator for the typed NIO buffers (Int/Long/Short/Float/
/// DoubleBuffer) — the abstract class itself declares no fields beyond
/// `Buffer`'s own 6 (`mark,position,limit,capacity,address,segment`), so a
/// real-JDK-shaped `java/nio/FloatBuffer` object ALSO has exactly 6 fields,
/// same as the pure-synthetic layout this function was written to detect.
/// Without the class-name check below, `s2_bb_order`/`s2_bb_set_order`
/// treated every typed-buffer view as pure-synthetic and read/wrote slot 5
/// as an int order flag — silently clobbering the array reference
/// `s2_typed_buffer_view_fns!`/`s2_view_buf_fn!` stash there (see
/// `s2_bb_arr`'s `BB_SEGMENT_SLOT` fallback), so every value read through a
/// typed-buffer view came back zero regardless of what was written.
///
/// **This predicate is NARROWER than "the indexed `BB_*` convention applies",
/// and callers must not read it as the wider claim.** A `--synthetic-jdk`
/// `java.nio.ByteBuffer` is 10 fields wide (`class_manager::synthetic_stub_fields`
/// gives `java/nio/ByteBuffer` `instance_fields(6)` over a `java/nio/Buffer`
/// with `instance_fields(4)`), so it fails the `== 6` screen while its slots
/// 0..5 still mean array/pos/limit/cap/mark/order. Reading `false` here as
/// "real JDK layout, so a name-keyed write is safe" is exactly the 2026-08-12
/// defect in [`s2_bb_set_order`]. Use [`s2_buf_stub_layout`] for the wider
/// question; the count screen is kept because several call sites below key an
/// aliasing-vs-copying decision on it, and widening it there would be a
/// behaviour change unrelated to byte order.
#[inline]
pub fn s2_bb_synthetic_layout(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    if ctx.object_num_fields(buf) != 6 {
        return false;
    }
    !s2_is_typed_buffer_view(ctx, buf)
}

/// The five abstract typed-buffer classes `s2_typed_buffer_view_fns!` /
/// `s2_view_buf_fn!` stamp on a view built by `ByteBuffer.as<T>Buffer()`.
///
/// These are the ONE family whose byte order lives in the indexed `BB_ARRAY`
/// slot: the view's backing array is stashed at [`BB_SEGMENT_SLOT`] (the only
/// Object-typed field `java.nio.Buffer` declares), which leaves slot 0 — real
/// `Buffer.mark`, an `int` — genuinely free for an order flag. Written down
/// once, here, so the reader (`s2_bb_order`), the writer (`s2_bb_set_order`)
/// and the layout screen (`s2_bb_synthetic_layout`) cannot drift apart on
/// which classes they mean; they used to carry three copies of this list
/// between them, and the discriminator each copy keyed on was different.
#[inline]
pub fn s2_is_typed_buffer_view(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(buf);
    matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some(
            "java/nio/IntBuffer"
                | "java/nio/LongBuffer"
                | "java/nio/ShortBuffer"
                | "java/nio/FloatBuffer"
                | "java/nio/DoubleBuffer"
        )
    )
}

/// True when `buf`'s class carries **no real `java.nio.Buffer` field
/// metadata** — i.e. the receiver is a fabricated synthetic-JDK stub (fields
/// `_f0.._fN`, all `Ljava/lang/Object;`) or a bare synthetic carrier, and its
/// slots therefore mean what the indexed `BB_*` constants say rather than what
/// a real JDK layout says.
///
/// **This is not the same question as [`s2_bb_synthetic_layout`], and that is
/// the whole point.** That predicate screens on `object_num_fields == 6`, and
/// a `--synthetic-jdk` `java.nio.ByteBuffer` is **10 fields wide**: the
/// fabricated `java/nio/ByteBuffer` stub declares `_f0.._f5` and its
/// fabricated `java/nio/Buffer` superclass declares `_f0.._f3`. Measured, not
/// inferred — `Class.forName("java.nio.ByteBuffer").getDeclaredFields()` under
/// `cratonvm --synthetic-jdk` prints exactly those ten, and the object's slots
/// 0..5 are the subclass's, so `native-io`'s `alloc_byte_buffer` parks the
/// backing array in slot 0 and the indexed `BB_*` convention holds unchanged.
/// The count screen reads that object as REAL-layout, which is how an `int`
/// order flag came to be written over the backing array (see
/// [`s2_bb_set_order`]).
///
/// The witness is CLASS-SIDE (`resolve_field_index_by_class_id`), the same
/// idiom `classloader::cl_has_synthetic_layout` uses, and deliberately not a
/// value-side `get_field_by_name(..) == Object(None)` probe: an absent name's
/// by-name READ is `Int(0)` under `MockNativeContext`, so a value-side witness
/// would answer "real layout" for every bare synthetic carrier under test and
/// the predicate would be untestable in the direction that matters.
///
/// `position` is the witness field because `java.nio.Buffer` declares it, so
/// every real buffer class in the hierarchy inherits it — `ByteBuffer`'s own
/// `bigEndian` would answer "synthetic" for a real `java.nio.CharBuffer`,
/// which is a real layout with no `bigEndian` of its own.
///
/// The width guard is not belt-and-braces: `native-io`'s `alloc_byte_buffer`
/// falls back to `alloc_object(ClassId(0), BB_NUM_FIELDS /* = 5 */)` when the
/// class will not resolve at all, and slot [`BB_ORDER`] is past the end of
/// that object.
#[inline]
pub fn s2_buf_stub_layout(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    ctx.object_num_fields(buf) > BB_ORDER
        && ctx
            .resolve_field_index_by_class_id(ctx.class_id_of_object(buf), "position")
            .is_none()
}

#[inline]
pub fn s2_bb_order(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    // Real-layout ByteBuffers keep their byte order in the named
    // `bigEndian` boolean (seeded to the JDK BIG_ENDIAN default by
    // `bb_write_hb`, mirrored by the `order(ByteOrder)` native below). The
    // indexed `BB_ORDER` slot (5) only exists on the pure-synthetic
    // layout — on a real-layout buffer that index aliases `segment`, so
    // the old slot read never yielded the order Int and every buffer
    // decoded as BIG_ENDIAN no matter what `order(LITTLE_ENDIAN)` was
    // called: Lucene's `BufferedIndexInput` (LE-ordered buffer from
    // `ByteBuffer.allocate(...).order(LITTLE_ENDIAN)`) then byteswapped
    // every buffered `getShort`/`getInt`/`getLong` — surfacing as
    // `CorruptIndexException: truncated file: length=79 but
    // expectedLength==5692549928996306944` (79 byte-reversed) in
    // `Lucene104PostingsReader` under the ES vector-codec tests.
    if s2_bb_synthetic_layout(ctx, buf) {
        return ctx.get_field(buf, BB_ORDER).as_int().unwrap_or(0);
    }
    // A real-JDK `java/nio/ByteBufferAs<T>Buffer{B,L}` carries its order in
    // the CLASS, not in a field: the JDK compiles one concrete view class per
    // endianness and `order()` is a constant return. It has no `bigEndian`
    // field, so the `BB_ARRAY`/`mark`-slot fallback below would decide by
    // whatever `mark` happens to hold — BIG_ENDIAN for the usual `mark == -1`,
    // which silently byteswaps every read through a `...BufferL` view. The
    // class name is exact; use it.
    let cname = ctx.class_name_arc_of_id(ctx.class_id_of_object(buf));
    if let Some(n) = cname.as_deref() {
        if let Some(tail) = n.strip_prefix("java/nio/ByteBufferAs") {
            if tail.ends_with('L') {
                return 1;
            }
            if tail.ends_with('B') {
                return 0;
            }
        }
    }
    match ctx.get_field_by_name(buf, "bigEndian") {
        Value::Int(v) => {
            if v != 0 {
                0
            } else {
                1
            }
        }
        // Unresolvable / non-Int on this non-synthetic buffer object. Three
        // distinct shapes land here and they do NOT share a storage slot —
        // see `s2_bb_set_order`, which must stay the exact mirror of this.
        _ => {
            if s2_is_typed_buffer_view(ctx, buf) {
                // Typed-buffer view: the order flag is stashed in the real
                // `mark: int` slot (index 0 — genuinely free for these views
                // once the array moved to `BB_SEGMENT_SLOT`; `Int`-into-`int`
                // is not subject to the descriptor-coercion trap that broke
                // slot 0 as Object storage). Any other value there (a stray
                // -1 default, or a genuine "no order set yet") falls back to
                // the JDK default BIG_ENDIAN.
                match ctx.get_field(buf, BB_ARRAY) {
                    Value::Int(1) => 1,
                    _ => 0,
                }
            } else if s2_buf_stub_layout(ctx, buf) {
                // Synthetic-JDK stub ByteBuffer/HeapByteBuffer/CharBuffer:
                // wider than 6 slots (so `s2_bb_synthetic_layout` above said
                // "real"), no `bigEndian` field to resolve, and slot 0 is the
                // BACKING ARRAY. The order goes where the name says it goes.
                ctx.get_field(buf, BB_ORDER).as_int().unwrap_or(0)
            } else {
                // A real JDK layout with neither a `bigEndian` field nor a
                // `ByteBufferAs…{B,L}` class name — e.g. a real
                // `java.nio.CharBuffer`. Reading slot 0 here would decode
                // whatever `Buffer.mark` happens to hold as a byte order. The
                // JDK default is the honest answer.
                0
            }
        }
    }
}

/// Write a buffer's byte order — the mutation counterpart of
/// `s2_bb_order`, with the same layout discrimination. Used by the
/// `order(ByteOrder)` native and by slice/view creation when propagating
/// the source buffer's order.
///
/// BUG (found 2026-08-12, `--synthetic-jdk`): the last branch used to be an
/// UNCONDITIONAL `set_field(buf, BB_ARRAY, Int(ord))`, and its comment stated
/// the premise it rested on — *"a genuine ByteBuffer … already round-trips
/// correctly by name"*. That premise is true of the real JDK class and FALSE
/// of the fabricated one: a synthetic-JDK `java.nio.ByteBuffer` is 10 fields
/// wide (`_f0.._f5` + `java/nio/Buffer._f0.._f3`, measured), so
/// `s2_bb_synthetic_layout`'s `== 6` screen called it real-layout, it has no
/// `bigEndian` field to resolve, and the write landed on slot 0 — the BACKING
/// ARRAY. `ByteBuffer.allocate(8).order(nativeOrder())` then took the VM down
/// with `internal error: ByteBuffer missing backing storage (… field 0
/// returned Int(1) …)`, exit 1, no Java exception.
///
/// It was invisible to the obvious probe because the corruption is
/// SELF-CONSISTENT: `s2_bb_order` read the same slot back, so `order()` still
/// reported `LITTLE_ENDIAN` over the destroyed buffer. Only the CONTENTS show
/// it. Assert bytes, never the reported order.
///
/// The three storage sites below are now named explicitly and each one is
/// gated on the shape that owns it, so no configuration reaches a slot whose
/// meaning it has not established. The unrecognised case REFUSES rather than
/// guessing: a byte order that silently fails to stick is a wrong value, and a
/// wrong value is recoverable; a scalar written over a reference slot is not.
pub fn s2_bb_set_order(ctx: &mut dyn NativeContext, buf: ObjectRef, ord: i32) {
    if s2_bb_synthetic_layout(ctx, buf) {
        ctx.set_field(buf, BB_ORDER, Value::Int(ord));
        return;
    }
    // Real ByteBuffer (has a genuine `bigEndian` field): write it by name,
    // as before.
    let has_big_endian_field =
        !matches!(ctx.get_field_by_name(buf, "bigEndian"), Value::Object(None));
    if has_big_endian_field {
        ctx.set_field_by_name(buf, "bigEndian", Value::Int(if ord == 1 { 0 } else { 1 }));
        // nativeByteOrder = (bigEndian == platform-is-big-endian); every
        // CratonVM target is little-endian, so it is "order == LITTLE_ENDIAN".
        ctx.set_field_by_name(
            buf,
            "nativeByteOrder",
            Value::Int(if ord == 1 { 1 } else { 0 }),
        );
    } else if s2_is_typed_buffer_view(ctx, buf) {
        // Typed-buffer view (IntBuffer/LongBuffer/ShortBuffer/FloatBuffer/
        // DoubleBuffer — no `bigEndian` field to resolve, unlike ByteBuffer):
        // the by-name write is a no-op, so stash the flag in the real
        // `mark: int` slot instead (index 0 — genuinely free for these views
        // once the array moved to `BB_SEGMENT_SLOT`). This is the ONLY shape
        // for which slot 0 is not the backing store, which is why it is now
        // gated on the class name rather than reached by falling through.
        ctx.set_field(buf, BB_ARRAY, Value::Int(ord));
    } else if s2_buf_stub_layout(ctx, buf) {
        // Synthetic-JDK stub ByteBuffer/HeapByteBuffer/CharBuffer. Slot 0 is
        // the backing array (`native-io`'s `alloc_byte_buffer` and
        // `s2_bb_alloc_direct` both put it there); [`BB_ORDER`] is the slot
        // this layout reserves for the flag, and it is free — `s2_bb_arr`
        // never reads it, and `native-io`'s `bb_resolve_heap_array` probes it
        // only for an `ObjectKind::Array` and falls through on an `Int`.
        ctx.set_field(buf, BB_ORDER, Value::Int(ord));
    }
    // Otherwise REFUSE. A real JDK layout with no `bigEndian` (a real
    // `java.nio.CharBuffer`, or a `ByteBufferAs…{B,L}` whose order is fixed by
    // its CLASS and cannot be reassigned at all) has no slot here that means
    // "byte order", and every candidate index aliases a field the real class
    // declares. Writing one would be the defect above with a different victim.
}
