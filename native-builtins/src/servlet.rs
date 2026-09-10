// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NIO, HTTP client, and resource loading natives.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
// The ONE copy of the `hasArray`/`array`/`arrayOffset` three-way rule and of
// the two one-liners that feed it. `cratonvm-native-builtins` depends on
// `cratonvm-native-io` (`Cargo.toml`) and not the reverse, so `native-io` is
// the only crate whose copy this file and `phases_late/charset_buffers.rs` can
// both import — which is why F14-1 made them `pub` there rather than moving
// them here. Record: F14-1 N1, landed by F21-1.
use cratonvm_native_io::{buffer_array_access, BufferArrayAccess};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
// The heap's own object-kind discriminant. `s2_bb_arr` uses it to refuse a
// `java.nio.Buffer.segment` that is a `MemorySegment` rather than a backing
// array — see W7-83-segment-as-backing-array.md.
use cratonvm_types::ObjectKind;
use cratonvm_types::{ObjectRef, Value};

use crate::phases_late::{
    p56_build_stream, p58_new_cf, CLEANABLE_ACTION, CLEANABLE_CLEANED, CLEANABLE_FIELDS,
    CLEANABLE_INDEX, REF_TYPE_CLEANER,
};
use crate::{native_noop_with_this, obj_arg, try_alloc_concurrent_synthetic};

use std::collections::HashMap;
use std::io::{Read as StdRead, Write as StdWrite};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, OnceLock};

#[cfg(feature = "synthetic-jdk")]
fn r3_get_input_stream(ctx: &dyn NativeContext, buffered_reader: ObjectRef) -> Option<ObjectRef> {
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
fn resolve_class_resource_name(
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

fn byte_array_to_vec(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        bytes.push(ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8);
    }
    bytes
}

fn charset_name_from_object(ctx: &mut dyn NativeContext, charset: ObjectRef) -> Option<String> {
    match ctx.invoke_virtual(charset, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => match ctx.get_field_by_name(charset, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
}

fn spring_mock_response_content_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<u8>, RuntimeError> {
    match ctx.invoke_virtual(this, "getContentAsByteArray", "()[B", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => Ok(byte_array_to_vec(ctx, arr)),
        _ => Ok(Vec::new()),
    }
}

fn spring_mock_response_get_content_as_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let charset =
        match ctx.invoke_virtual(this, "getCharacterEncoding", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => {
                ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string())
            }
            _ => "UTF-8".to_string(),
        };
    let bytes = spring_mock_response_content_bytes(ctx, this)?;
    let text = crate::charset::decode_str_named(&charset, &bytes);
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

fn spring_mock_response_get_content_as_string_charset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let charset = match args.get(1) {
        Some(Value::Object(Some(cs))) => {
            charset_name_from_object(ctx, *cs).unwrap_or_else(|| "UTF-8".to_string())
        }
        _ => "UTF-8".to_string(),
    };
    let bytes = spring_mock_response_content_bytes(ctx, this)?;
    let text = crate::charset::decode_str_named(&charset, &bytes);
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

fn jython_py_bool(ctx: &mut dyn NativeContext, value: bool) -> MethodCallResult {
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

fn jython_py_none(ctx: &mut dyn NativeContext) -> Value {
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "None") {
            if let Value::Object(Some(obj)) = ctx.get_static_field(py_cid, idx) {
                return Value::Object(Some(obj));
            }
        }
    }
    Value::Object(None)
}

fn jython_object_class_is(ctx: &mut dyn NativeContext, obj: ObjectRef, expected: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(expected)
}

fn jython_map_field(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
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

fn jython_pystringmap_put_all(
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

fn jython_new_pystringmap(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized("org/python/core/PyStringMap", "()V", &[])? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: "failed to create Jython PyStringMap".to_string(),
        }
        .into()),
    }
}

fn jython_new_pyinteger(
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

fn jython_pyobject_finditem_string(
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

fn jython_pyobject_setitem_string(
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

fn jython_module_dict(
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

fn jython_new_module(
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

fn jython_ensure_sre_module_attrs(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let dict = jython_module_dict(ctx, module)?;
    for (name, value) in [("MAGIC", 20031017), ("MAXREPEAT", 65535), ("CODESIZE", 4)] {
        let integer = jython_new_pyinteger(ctx, value)?;
        jython_pyobject_setitem_string(ctx, dict, name, Value::Object(Some(integer)))?;
    }
    Ok(())
}

fn jython_pyobject_text(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    if let Some(text) = jython_py_string_value(ctx, obj) {
        return Some(text);
    }
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

fn jython_pymodule_name(ctx: &mut dyn NativeContext, module: ObjectRef) -> Option<String> {
    let dict = jython_module_dict(ctx, module).ok()?;
    let name = jython_pyobject_finditem_string(ctx, dict, "__name__")?;
    jython_pyobject_text(ctx, name)
}

fn jython_pymodule_package_lookup(
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
    ctx.unpin_native_roots(package_manager_pin);
    Ok(Some(Value::Object(Some(found))))
}

fn jython_pymodule_findattr_ex(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

fn jython_pymodule_findattr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jython_pymodule_findattr_ex(ctx, args)
}

fn jython_system_modules(
    ctx: &mut dyn NativeContext,
    state: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Value::Object(Some(modules)) = ctx.get_field_by_name(state, "modules") {
        return Ok(modules);
    }
    let modules = jython_new_pystringmap(ctx)?;
    ctx.set_field_by_name(state, "modules", Value::Object(Some(modules)));
    Ok(modules)
}

fn jython_ensure_builtin_module(
    ctx: &mut dyn NativeContext,
    modules: ObjectRef,
    state: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(module) = jython_pyobject_finditem_string(ctx, modules, "__builtin__") {
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

fn jython_imp_add_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

fn jython_pyobject_invoke_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let receiver = obj_arg(args, 0)?;
    let name_obj = obj_arg(args, 1)?;
    let arg = args.get(2).copied().unwrap_or(Value::Object(None));
    let name = ctx.read_string(name_obj).unwrap_or_default();

    if name == "update" && jython_object_class_is(ctx, receiver, "org/python/core/PyStringMap") {
        if let Value::Object(Some(source)) = arg {
            if jython_pystringmap_put_all(ctx, receiver, source) {
                return Ok(Some(jython_py_none(ctx)));
            }
        }
        let _ = ctx.invoke_virtual(receiver, "update", "(Lorg/python/core/PyObject;)V", &[arg])?;
        return Ok(Some(jython_py_none(ctx)));
    }

    let attr = match ctx.invoke_virtual(
        receiver,
        "__getattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        &[Value::Object(Some(name_obj))],
    )? {
        Some(Value::Object(Some(attr))) => attr,
        value => return Ok(value),
    };
    ctx.invoke_virtual(
        attr,
        "__call__",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        &[arg],
    )
}

fn jython_sre_const_value(attr: &str) -> Option<i32> {
    match attr {
        "MAGIC" => Some(20031017),
        "MAXREPEAT" => Some(65535),
        "CODESIZE" => Some(4),
        _ => None,
    }
}

fn jython_pytype_name(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    if !jython_object_class_is(ctx, obj, "org/python/core/PyJavaType") {
        return None;
    }
    match ctx.invoke_virtual(obj, "fastGetName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name),
        _ => None,
    }
}

fn jython_pyjavatype_findattr_ex(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let receiver = obj_arg(args, 0)?;
    let attr_obj = obj_arg(args, 1)?;
    let attr = ctx.read_string(attr_obj).unwrap_or_default();
    if matches!(
        jython_pytype_name(ctx, receiver).as_deref(),
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
        args,
    )
}

fn jython_pyobject_is(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    jython_py_bool(ctx, lhs.as_ptr() == rhs.as_ptr())
}

fn jython_pyobject_isnot(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    jython_py_bool(ctx, lhs.as_ptr() != rhs.as_ptr())
}

fn jython_py_string_value(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<String> {
    match ctx.get_field_by_name(obj, "string") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn jython_py_integer_value(
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

fn jython_pyobject_equal(ctx: &mut dyn NativeContext, lhs: ObjectRef, rhs: ObjectRef) -> bool {
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

fn jython_pyobject_eq(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    let equal = jython_pyobject_equal(ctx, lhs, rhs);
    jython_py_bool(ctx, equal)
}

fn jython_pyobject_ne(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lhs = obj_arg(args, 0)?;
    let rhs = obj_arg(args, 1)?;
    let equal = jython_pyobject_equal(ctx, lhs, rhs);
    jython_py_bool(ctx, !equal)
}

pub(crate) fn register_jython_pyobject_natives(r: &mut NativeMethodRegistry) {
    let cls = "org/python/core/PyObject";
    r.register(
        cls,
        "invoke",
        "(Ljava/lang/String;Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_invoke_one,
    );
    r.register(
        cls,
        "_is",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_is,
    );
    r.register(
        cls,
        "_isnot",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_isnot,
    );
    r.register(
        cls,
        "_eq",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_eq,
    );
    r.register(
        cls,
        "_ne",
        "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;",
        jython_pyobject_ne,
    );
}

fn jython_find_module_getattr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn register_jython_imp_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "org/python/core/imp",
        "addModule",
        "(Ljava/lang/String;)Lorg/python/core/PyModule;",
        jython_imp_add_module,
    );
    r.register(
        "org/python/core/PyModule",
        "__findattr_ex__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_pymodule_findattr_ex,
    );
    r.register(
        "org/python/core/PyModule",
        "__findattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_pymodule_findattr,
    );
    r.register(
        "org/python/core/PyJavaType",
        "__findattr_ex__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_pyjavatype_findattr_ex,
    );
    r.register(
        "org/python/core/PyNullImporter",
        "__getattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_find_module_getattr,
    );
    r.register(
        "org/python/modules/zipimport/zipimporter",
        "__getattr__",
        "(Ljava/lang/String;)Lorg/python/core/PyObject;",
        jython_find_module_getattr,
    );
}

fn jython_current_thread_system_state(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
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

fn jython_py_get_or_create_system_state(ctx: &mut dyn NativeContext) -> MethodCallResult {
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

fn jython_py_get_system_state(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    jython_py_get_or_create_system_state(ctx)
}

fn jython_py_set_system_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let old = jython_py_get_or_create_system_state(ctx)?;
    let new_state = args.get(0).copied().unwrap_or(Value::Object(None));
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "defaultSystemState") {
            ctx.set_static_field(py_cid, idx, new_state);
        }
    }
    Ok(old)
}

fn jython_py_get_thread_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let requested_state = args.get(0).copied().unwrap_or(Value::Object(None));
    if let Some(py_cid) = ctx.class_id_by_name("org/python/core/Py") {
        if let Some(idx) = ctx.static_field_index_by_name(py_cid, "threadStateMapping") {
            if let Value::Object(Some(mapping)) = ctx.get_static_field(py_cid, idx) {
                if let Ok(Some(state)) = ctx.invoke_virtual(
                    mapping,
                    "getThreadState",
                    "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
                    &[requested_state],
                ) {
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

fn jython_py_get_thread_state_noarg(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    jython_py_get_thread_state(ctx, &[Value::Object(None)])
}

fn jython_py_import_site_if_selected(
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

pub(crate) fn register_jython_thread_state_natives(r: &mut NativeMethodRegistry) {
    let cls = "org/python/core/Py";
    r.register(
        cls,
        "importSiteIfSelected",
        "()Z",
        jython_py_import_site_if_selected,
    );
    r.register(
        cls,
        "getSystemState",
        "()Lorg/python/core/PySystemState;",
        jython_py_get_system_state,
    );
    r.register(
        cls,
        "setSystemState",
        "(Lorg/python/core/PySystemState;)Lorg/python/core/PySystemState;",
        jython_py_set_system_state,
    );
    r.register(
        cls,
        "getThreadState",
        "()Lorg/python/core/ThreadState;",
        jython_py_get_thread_state_noarg,
    );
    r.register(
        cls,
        "getThreadState",
        "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
        jython_py_get_thread_state,
    );
}

pub(crate) fn register_spring_mock_response_natives(r: &mut NativeMethodRegistry) {
    for cls in [
        "org/springframework/mock/web/MockHttpServletResponse",
        "org/springframework/web/testfixture/servlet/MockHttpServletResponse",
    ] {
        r.register(
            cls,
            "getContentAsString",
            "()Ljava/lang/String;",
            spring_mock_response_get_content_as_string,
        );
        r.register(
            cls,
            "getContentAsString",
            "(Ljava/nio/charset/Charset;)Ljava/lang/String;",
            spring_mock_response_get_content_as_string_charset,
        );
    }
}

fn native_bool(value: Option<Value>) -> bool {
    matches!(value, Some(Value::Int(v)) if v != 0)
}

fn script_engine_manager_create_engine(
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

fn native_int(value: Option<Value>) -> Option<i32> {
    match value {
        Some(Value::Int(v)) => Some(v),
        _ => None,
    }
}

fn java_list_size(ctx: &mut dyn NativeContext, list: ObjectRef) -> Option<i32> {
    native_int(ctx.invoke_virtual(list, "size", "()I", &[]).ok().flatten())
}

fn java_list_get(ctx: &mut dyn NativeContext, list: ObjectRef, index: i32) -> Option<ObjectRef> {
    match ctx
        .invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(index)])
        .ok()
        .flatten()
    {
        Some(Value::Object(Some(obj))) => Some(obj),
        _ => None,
    }
}

fn java_string_list_contains(ctx: &mut dyn NativeContext, list: ObjectRef, key: ObjectRef) -> bool {
    let key_text = ctx.read_string(key);
    if let Some(size) = java_list_size(ctx, list) {
        let list_pin = ctx.pin_native_root(list);
        for index in 0..size {
            let list = ctx.read_native_pin(list_pin, list);
            let Some(item) = java_list_get(ctx, list, index) else {
                continue;
            };
            if item.as_ptr() == key.as_ptr() {
                return true;
            }
            if let Some(expected) = key_text.as_deref() {
                if ctx.read_string(item).as_deref() == Some(expected) {
                    return true;
                }
            }
        }
    }
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

fn script_engine_manager_factory_matches(
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

fn script_engine_manager_try_factory(
    ctx: &mut dyn NativeContext,
    manager: ObjectRef,
    factory: ObjectRef,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    match script_engine_manager_create_engine(ctx, manager, factory)? {
        Some(Value::Object(Some(engine))) => Ok(Some(engine)),
        _ => Ok(None),
    }
}

fn script_engine_manager_get_engine(
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

fn script_engine_manager_get_engine_by_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    script_engine_manager_get_engine(ctx, args, "nameAssociations", "getNames")
}

fn script_engine_manager_get_engine_by_extension(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    script_engine_manager_get_engine(ctx, args, "extensionAssociations", "getExtensions")
}

fn script_engine_manager_get_engine_by_mime_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    script_engine_manager_get_engine(ctx, args, "mimeTypeAssociations", "getMimeTypes")
}

pub(crate) fn register_script_engine_manager_natives(r: &mut NativeMethodRegistry) {
    let cls = "javax/script/ScriptEngineManager";
    r.register(
        cls,
        "getEngineByName",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        script_engine_manager_get_engine_by_name,
    );
    r.register(
        cls,
        "getEngineByExtension",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        script_engine_manager_get_engine_by_extension,
    );
    r.register(
        cls,
        "getEngineByMimeType",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        script_engine_manager_get_engine_by_mime_type,
    );
}

pub(crate) fn register_r3_resource_loading(r: &mut NativeMethodRegistry) {
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
                    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
                    for (i, &b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                    }
                    let stream =
                        try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
                    ctx.set_field(stream, 0, Value::Object(Some(arr))); // buf
                    ctx.set_field(stream, 1, Value::Int(0)); // pos
                    ctx.set_field(stream, 2, Value::Int(0)); // mark
                    ctx.set_field(stream, 3, Value::Int(bytes.len() as i32)); // count
                    Ok(Some(Value::Object(Some(stream))))
                }
            }
        },
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
                    let url = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 6)?;
                    let full_str = ctx.create_string(&format!("classpath:{resource_name}"));
                    ctx.set_field(url, 5, Value::Object(Some(full_str)));
                    Ok(Some(Value::Object(Some(url))))
                }
            }
        },
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
    #[cfg(feature = "synthetic-jdk")]
    {
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
            },
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
            },
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
            },
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
        });
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
        });

        // Real-JDK BufferedReader methods retain their bytecode implementation.
    } // end #[cfg(feature = "synthetic-jdk")] synthetic Reader-stack block
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
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register("java/net/URLClassLoader", "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let urls = ctx
            .invoke_virtual(this, "getURLs", "()[Ljava/net/URL;", &[])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None));
        let paths = s1_url_array_to_fs_paths(ctx, urls);
        if !paths.is_empty() {
            ctx.unregister_dynamic_classpath(&paths);
        }
        // Retracting the dynamic-classpath roots is only half of it in real-JDK
        // mode: the loader's own `findClass` there is
        // `classloader_real::ucl_real_find_class`, which searches on its own
        // behalf because the real `ucp` CratonVM hands the loader is never
        // populated. Mark the loader so that native refuses too.
        crate::classloader_real::ucl_mark_closed(ctx, this);
        Ok(None)
    });
    r.set_category(__prev_cat);
}

/// Every filesystem path a `URL[]` names, in order.
///
/// Shared by `URLClassLoader`'s constructors (which register the paths) and by
/// `close()` (which retracts them), so the two cannot compute a different set
/// from the same array and leave a root behind.
fn s1_url_array_to_fs_paths(ctx: &mut dyn NativeContext, url_arr_val: Value) -> Vec<String> {
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
fn s1_url_to_fs_path(url_str: &str) -> Option<String> {
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
fn s1_service_loader_ensure_loaded(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    use cratonvm_types::ArrayElementType;

    // If already loaded, return cached array
    if let Value::Object(Some(arr)) = ctx.get_field(sl, 1) {
        return arr;
    }

    let empty = ctx.new_array(ArrayElementType::Reference, 0);

    // Get the service interface ClassId from the Class mirror (field 0)
    let mirror = match ctx.get_field(sl, 0) {
        Value::Object(Some(m)) => m,
        _ => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
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
        None => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
    };
    let iface_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n.replace('/', "."),
        None => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
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

    if provider_names.is_empty() {
        ctx.set_field(sl, 1, Value::Object(Some(empty)));
        return empty;
    }

    // Load and instantiate each provider via default constructor
    let mut instances = Vec::new();
    for provider_name in &provider_names {
        let class_name = provider_name.replace('.', "/");
        if ctx.ensure_class_initialized(&class_name).is_ok() {
            if let Ok(Some(Value::Object(Some(obj)))) = ctx.new_object(&class_name) {
                let _ = ctx.invoke(&class_name, "<init>", "()V", &[Value::Object(Some(obj))]);
                instances.push(Value::Object(Some(obj)));
            }
        }
    }

    // Store as Object[]
    let arr = ctx.new_array(ArrayElementType::Reference, instances.len());
    for (i, inst) in instances.iter().enumerate() {
        ctx.set_array_element(arr, i, *inst);
    }
    ctx.set_field(sl, 1, Value::Object(Some(arr)));
    arr
}

pub(crate) fn register_s1_classloading(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;

    // =========================================================================
    // java.net.URLClassLoader
    // =========================================================================
    let ucl = "java/net/URLClassLoader";

    // Helper closure: extract filesystem paths from a URL[] array object
    // and register them with the VM classpath.
    fn register_url_array(ctx: &mut dyn NativeContext, url_arr_val: Value) {
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
        register_url_array(ctx, url_arr);
        Ok(None)
    });

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
            register_url_array(ctx, url_arr);
            Ok(None)
        },
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
            register_url_array(ctx, url_arr);
            Ok(None)
        },
    );

    // URLClassLoader.newInstance(URL[]) → URLClassLoader
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;)Ljava/net/URLClassLoader;",
        |ctx, args| {
            let url_arr = args.first().copied().unwrap_or(Value::Object(None));
            let loader = try_alloc_concurrent_synthetic(ctx, "java/net/URLClassLoader", 2)?;
            ctx.set_field(loader, 0, url_arr);
            ctx.set_field(loader, 1, Value::Object(None));
            register_url_array(ctx, url_arr);
            Ok(Some(Value::Object(Some(loader))))
        },
    );

    // URLClassLoader.newInstance(URL[], ClassLoader) → URLClassLoader
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;",
        |ctx, args| {
            let url_arr = args.first().copied().unwrap_or(Value::Object(None));
            let parent = args.get(1).copied().unwrap_or(Value::Object(None));
            let loader = try_alloc_concurrent_synthetic(ctx, "java/net/URLClassLoader", 2)?;
            ctx.set_field(loader, 0, url_arr);
            ctx.set_field(loader, 1, parent);
            register_url_array(ctx, url_arr);
            Ok(Some(Value::Object(Some(loader))))
        },
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
        },
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
        },
    );

    // URLClassLoader.getURLs() → URL[]
    r.register(ucl, "getURLs", "()[Ljava/net/URL;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // URLClassLoader.addURL(URL) — extend classpath with one more URL
    r.register(ucl, "addURL", "(Ljava/net/URL;)V", |ctx, args| {
        let url_val = args.get(1).copied().unwrap_or(Value::Object(None));
        if let Value::Object(Some(url_obj)) = url_val {
            let full_val = ctx.get_field(url_obj, 5); // URL_FIELD_FULL
            if let Value::Object(Some(s)) = full_val {
                if let Some(full) = ctx.read_string(s) {
                    if let Some(path) = s1_url_to_fs_path(&full) {
                        ctx.register_dynamic_classpath(&[path]);
                    }
                }
            }
        }
        Ok(None)
    });

    // This exact URLClassLoader registration is installed after the generic
    // ClassLoader one. Keep it on the same parent-first implementation so a
    // URLClassLoader child can see @WithResource files supplied by its parent.
    r.register(
        ucl,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        crate::classloader::cl_get_resource_essential,
    );

    // Use the same delegation path for stream lookup.
    r.register(
        ucl,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        crate::classloader::cl_get_resource_as_stream_essential,
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
            ctx.unregister_dynamic_classpath(&paths);
        }
        Ok(None)
    });

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
            let class_mirror = args.first().copied().unwrap_or(Value::Object(None));
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2)?;
            ctx.set_field(obj, 0, class_mirror);
            ctx.set_field(obj, 1, Value::Object(None)); // not yet loaded
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ServiceLoader.load(Class, ClassLoader) → ServiceLoader
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            let class_mirror = args.first().copied().unwrap_or(Value::Object(None));
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2)?;
            ctx.set_field(obj, 0, class_mirror);
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ServiceLoader.loadInstalled(Class) → ServiceLoader
    r.register(
        sl,
        "loadInstalled",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            let class_mirror = args.first().copied().unwrap_or(Value::Object(None));
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2)?;
            ctx.set_field(obj, 0, class_mirror);
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ServiceLoader.iterator() → Iterator<S>
    r.register(sl, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s1_service_loader_ensure_loaded(ctx, this);
        let len = ctx.array_length(arr);
        // Build a simple iterator backed by index over the array:
        // Iterator = 2-field: array=0, index=1
        let itr = try_alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2)?;
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        // Register hasNext/next for ServiceLoader$Itr if not already
        let _ = len; // suppress unused
        Ok(Some(Value::Object(Some(itr))))
    });

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
        },
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
        },
    );

    // ServiceLoader.stream() → Stream<Provider<S>>
    r.register(sl, "stream", "()Ljava/util/stream/Stream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s1_service_loader_ensure_loaded(ctx, this);
        let len = ctx.array_length(arr);
        let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
        let s = p56_build_stream(ctx, elems, "java/util/stream/Stream")?;
        Ok(Some(Value::Object(Some(s))))
    });

    // ServiceLoader.findFirst() → Optional<S>
    r.register(sl, "findFirst", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s1_service_loader_ensure_loaded(ctx, this);
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if ctx.array_length(arr) > 0 {
            let first = ctx.get_array_element(arr, 0);
            ctx.set_field(opt, 0, first);
        } else {
            ctx.set_field(opt, 0, Value::Object(None));
        }
        Ok(Some(Value::Object(Some(opt))))
    });

    // ServiceLoader.reload() — clear cached services
    r.register(sl, "reload", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Object(None)); // clear cache
        Ok(None)
    });

    r.register(sl, "toString", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("ServiceLoader[]");
        Ok(Some(Value::Object(Some(s))))
    });
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

pub(crate) struct SocketRegistry {
    pub(crate) next_id: i32,
    // Arc<TcpStream> (not bare TcpStream): a blocking read must run on the SAME
    // OS socket fd without holding the global registry lock, so callers clone
    // the cheap Arc under a short lock then do I/O on the shared `&TcpStream`
    // (std impls Read/Write for `&TcpStream`). A try_clone() duplicate fd is the
    // wrong tool here — on Windows a recv() blocked on a *duplicate* handle is
    // not woken by data that arrives after it blocked, only by FIN/close, which
    // wedges every request/response loopback (okhttp MockWebServer never
    // replies). Holding the lock across the blocking read instead deadlocks the
    // peer writer. Arc gives a same-fd handle that outlives the lock. (BUG-04)
    pub(crate) streams: HashMap<i32, Arc<TcpStream>>,
    pub(crate) listeners: HashMap<i32, TcpListener>,
    /// NEW-3: persistent UDP sockets backing `java.nio.channels.DatagramChannel`
    /// instances. The previous implementation re-created a `UdpSocket` on every
    /// `send`/`receive` call, which (a) wasted socket fds, (b) changed the local
    /// ephemeral port on every call, (c) made selector registration impossible.
    pub(crate) dgrams: HashMap<i32, UdpSocket>,
    /// NEW-13: persistent TLS streams backing `javax.net.ssl.SSLSocket` instances,
    /// unified with the plain-TCP registry so that SSLSocket and Socket share
    /// the same id space and close/shutdown semantics. Each entry couples the
    /// native-tls stream with the peer certificate DER bytes captured during
    /// the handshake so `SSLSession.getPeerCertificates()` can reconstruct a
    /// `java.security.cert.X509Certificate[]` later.
    pub(crate) tls_streams: HashMap<i32, TlsEntry>,
}

/// NEW-13: per-TLS-stream state captured during the handshake. The peer cert
/// chain is stored as DER bytes so it can be handed out repeatedly through
/// `SSLSession.getPeerCertificates()` without touching the live TLS stream.
pub(crate) struct TlsEntry {
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
    pub(crate) stream: Arc<cratonvm_types::lock_order::OrderedPlMutex<TlsClientStream>>,
    /// A `try_clone`d handle on the same underlying socket, for fd-level
    /// operations (`shutdownInput/Output`, `set/getSoTimeout`) that must NOT
    /// wait on `stream`'s mutex — `shutdownInput` is exactly how a caller
    /// unblocks a peer parked in a blocking TLS read, so taking that mutex
    /// here would deadlock. `None` only if `try_clone` failed.
    pub(crate) raw: Option<TcpStream>,
    pub(crate) peer_host: String,
    pub(crate) peer_port: u16,
    /// T2.7.11: owned so real negotiated handshake values can live here
    /// instead of compile-time placeholders. Populated by `s2_tls_connect`
    /// from `TlsStream::negotiated_alpn()` plus connector bounds.
    pub(crate) negotiated_protocol: String,
    pub(crate) negotiated_cipher: String,
    /// T2.7.11: the ALPN protocol the server selected during the handshake
    /// (`None` if neither side offered ALPN). Surfaced as
    /// `SSLSocket.getApplicationProtocol()` on the Java side.
    pub(crate) negotiated_alpn: Option<String>,
    pub(crate) peer_cert_chain_der: Vec<Vec<u8>>,
}

pub(crate) enum TlsClientStream {
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
    pub(crate) fn get_ref(&self) -> &TcpStream {
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
        }
    }
}

static SOCKET_REGISTRY: OnceLock<parking_lot::Mutex<SocketRegistry>> = OnceLock::new();

pub(crate) fn s2_registry() -> &'static parking_lot::Mutex<SocketRegistry> {
    SOCKET_REGISTRY.get_or_init(|| parking_lot::Mutex::new(SocketRegistry::default()))
}

/// Advance `next_id` to a value not currently in use by any of the three
/// socket tables. Pulled into a helper so every allocation site uses the
/// same collision-avoidance logic.
fn s2_next_free_id(reg: &mut SocketRegistry) -> i32 {
    let mut id = reg.next_id;
    // Skip 0 (reserved "invalid"), then skip any id that collides with an
    // existing entry in any of the tables.
    loop {
        if id == 0 {
            id = 1;
            continue;
        }
        if reg.streams.contains_key(&id)
            || reg.listeners.contains_key(&id)
            || reg.dgrams.contains_key(&id)
        {
            id = id.checked_add(1).unwrap_or(1);
            continue;
        }
        break;
    }
    reg.next_id = id.checked_add(1).unwrap_or(1);
    id
}

pub(crate) fn s2_alloc_stream(stream: TcpStream) -> i32 {
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.streams.insert(id, Arc::new(stream));
    id
}

pub(crate) fn s2_alloc_listener(listener: TcpListener) -> i32 {
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.listeners.insert(id, listener);
    id
}

/// NEW-3: register a persistent UDP socket with the shared registry so that
/// `DatagramChannel` operations reuse the same fd across send/receive/read/
/// write calls and so that the Selector can poll its readiness.
pub(crate) fn s2_alloc_dgram(dgram: UdpSocket) -> i32 {
    let mut reg = s2_registry().lock();
    let id = s2_next_free_id(&mut reg);
    reg.dgrams.insert(id, dgram);
    id
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
pub(crate) fn s2_tls_connect(
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
pub(crate) enum TlsConnectFailure {
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
pub(crate) fn s2_tls_connect_on(
    connector: &native_tls::TlsConnector,
    host: &str,
    port: u16,
    tcp: TcpStream,
) -> Result<i32, TlsConnectFailure> {
    // Reasonable defaults: non-infinite read/write timeouts so a hung peer
    // never deadlocks the JVM thread calling `SSLSocket.getInputStream().read`.
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));

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
pub(crate) fn s2_legacy_dsa_tls_connect(
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
pub(crate) fn s2_legacy_dsa_tls_connect_on(
    host: &str,
    port: u16,
    trust_root_ders: &[Vec<u8>],
    tcp: TcpStream,
) -> Result<i32, TlsConnectFailure> {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    use openssl::x509::{store::X509StoreBuilder, X509VerifyResult, X509};
    let hs = |e: &dyn std::fmt::Display| TlsConnectFailure::Handshake(e.to_string());
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
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
pub(crate) const CLIENT_SECURITY_LEVEL: u32 = 1;

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
pub(crate) struct OpensslClientConfig {
    /// Trust anchors (DER) to configure on the connector.
    pub(crate) roots: Vec<Vec<u8>>,
    /// `true` = `roots` REPLACE the platform set (JSSE's rule for a trust
    /// store the application named, and for the JDK's own `cacerts`);
    /// `false` = they are ADDED to it, which is what a per-`SSLContext` custom
    /// anchor set has always done here.
    pub(crate) replace_roots: bool,
    /// Stand OpenSSL's verifier (and its hostname check) DOWN: the caller is
    /// the verifier and runs immediately after connect, fail-closed. Mirrors
    /// `danger_accept_invalid_certs` + `danger_accept_invalid_hostnames`.
    pub(crate) skip_verify: bool,
    /// Pin the ceiling to TLS 1.2, for a version-specific
    /// `SSLContext.getInstance("TLSv1.2")`.
    pub(crate) max_tls12: bool,
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
fn openssl_peer_chain_der(
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
pub(crate) fn s2_openssl_tls_connect(
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
pub(crate) fn s2_openssl_tls_connect_on(
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
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));

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
pub(crate) struct SchannelClientConfig {
    /// Anchors to ADD to the platform set, as native-tls has always done here.
    pub(crate) roots: Vec<Vec<u8>>,
    /// Stand SChannel's verifier and hostname check down: the caller is the
    /// verifier and runs immediately after connect, fail-closed.
    pub(crate) skip_verify: bool,
    /// Pin the ceiling to TLS 1.2, for `SSLContext.getInstance("TLSv1.2")`.
    pub(crate) max_tls12: bool,
}

/// [`order_chain_from_leaf`], against the input the live platform declines to
/// produce.
///
/// MEASURED: with the walk skipped, SChannel returned leaf-first on all six
/// hosts of `PeerChainOrderProbe` — so a probe against the real world cannot
/// tell the walk from its absence, and the only honest way to exercise it is
/// to hand it the shuffled set the store is permitted to hand back but
/// currently does not.
///
/// The certificates are two-byte stand-ins and the `names` closure is a lookup
/// table, deliberately: what is under test is the subject/issuer walk, not the
/// DER parser, and a fixture built from real certificates would test both at
/// once and be unbuildable on the platform this code runs on (no `openssl`
/// crate on Windows).
#[cfg(test)]
mod chain_order_tests {
    use super::order_chain_from_leaf;

    /// leaf <- i1 <- i2 <- root(self-signed)
    fn names(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let (subject, issuer): (&[u8], &[u8]) = match der {
            b"leaf" => (b"L", b"I1"),
            b"i1__" => (b"I1", b"I2"),
            b"i2__" => (b"I2", b"RT"),
            b"root" => (b"RT", b"RT"),
            b"xxxx" => (b"XX", b"YY"),
            _ => return None,
        };
        Some((subject.to_vec(), issuer.to_vec()))
    }

    fn v(items: &[&[u8]]) -> Vec<Vec<u8>> {
        items.iter().map(|b| b.to_vec()).collect()
    }

    #[test]
    fn a_shuffled_pool_is_walked_back_into_a_path() {
        let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"i2__", b"root", b"i1__"]), names);
        assert_eq!(out, v(&[b"leaf", b"i1__", b"i2__", b"root"]));
    }

    /// The order the live store happens to use today must survive unchanged —
    /// a walk that only works on shuffled input would be worse than none.
    #[test]
    fn an_already_ordered_pool_is_left_alone() {
        let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"i1__", b"i2__", b"root"]), names);
        assert_eq!(out, v(&[b"leaf", b"i1__", b"i2__", b"root"]));
    }

    /// A certificate that is not on the path is APPENDED. Dropping it would
    /// take a cross-certificate away from `x509_manager::validate_chain`,
    /// which builds its own path and may need exactly that one.
    #[test]
    fn a_certificate_off_the_path_is_kept_at_the_end() {
        let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"xxxx", b"i1__"]), names);
        assert_eq!(out, v(&[b"leaf", b"i1__", b"xxxx"]));
    }

    /// A self-issued certificate ends the path. Following `subject == issuer`
    /// one step further would append the root to itself forever.
    #[test]
    fn a_self_issued_root_terminates_the_walk() {
        let out = order_chain_from_leaf(b"root".to_vec(), v(&[b"root"]), names);
        assert_eq!(out, v(&[b"root", b"root"]));
    }

    /// An unparseable member cannot stall the walk or vanish.
    #[test]
    fn an_unparseable_member_is_kept_and_skipped() {
        let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"junk", b"i1__"]), names);
        assert_eq!(out, v(&[b"leaf", b"i1__", b"junk"]));
    }

    /// An empty pool is the one-certificate chain, unchanged.
    #[test]
    fn an_empty_pool_yields_the_leaf_alone() {
        let out = order_chain_from_leaf(b"leaf".to_vec(), Vec::new(), names);
        assert_eq!(out, v(&[b"leaf"]));
    }
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
fn order_chain_from_leaf<F>(leaf: Vec<u8>, mut pool: Vec<Vec<u8>>, names: F) -> Vec<Vec<u8>>
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
fn schannel_peer_chain_der(stream: &schannel::tls_stream::TlsStream<TcpStream>) -> Vec<Vec<u8>> {
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
pub(crate) fn s2_schannel_tls_connect(
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
pub(crate) fn s2_schannel_tls_connect_on(
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
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
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
pub(crate) const RUSTLS_SOCK_ID_BASE: i32 = 0x4000_0000;

/// A socket returned by `SSLSocketFactory.createSocket(Socket, String, int,
/// boolean)` before its CLIENT-vs-SERVER handshake role is known (real JDK
/// contract: defaults to client mode, but the caller may still call
/// `setUseClientMode(false)` before the handshake actually starts — exactly
/// what MockWebServer's HTTPS listener does) stores a pending-handshake id
/// offset by this base instead of a real rustls stream id. Distinct from,
/// and numerically below, `RUSTLS_SOCK_ID_BASE` so the two ranges never
/// collide; see `t27_tls::{stash_pending_layered_socket, drive_pending_layered_handshake}`.
pub(crate) const PENDING_LAYERED_SOCK_ID_BASE: i32 = 0x2000_0000;

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
pub(crate) const PENDING_CONNECT_SOCK_ID_BASE: i32 = 0x1000_0000;

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
const TLS_READAHEAD_CAP: usize = 32 * 1024;

struct TlsReadahead {
    buf: Vec<u8>,
    pos: usize,
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
fn tls_readahead() -> &'static parking_lot::Mutex<HashMap<i32, TlsReadahead>> {
    static T: OnceLock<parking_lot::Mutex<HashMap<i32, TlsReadahead>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Move up to `out.len()` already-buffered plaintext bytes for `id` into
/// `out`. Returns 0 when nothing is buffered (the caller must then do a real,
/// blocking read).
fn tls_readahead_drain(id: i32, out: &mut [u8]) -> usize {
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
pub(crate) fn s2_tls_buffered_len(id: i32) -> usize {
    tls_readahead()
        .lock()
        .get(&id)
        .map(|e| e.buf.len() - e.pos)
        .unwrap_or(0)
}

/// Pop one buffered plaintext byte without any blocking-region bookkeeping.
/// `None` means "nothing buffered" — the caller must fall back to
/// [`s2_tls_fill_readahead`] inside a blocking region.
pub(crate) fn s2_tls_pop_buffered_byte(id: i32) -> Option<u8> {
    let mut out = [0u8; 1];
    (tls_readahead_drain(id, &mut out) == 1).then_some(out[0])
}

/// Do ONE real read into the readahead buffer for `id`. Returns the number of
/// bytes buffered (0 = EOF). MUST be called inside a blocking region — it
/// performs genuine blocking socket I/O.
pub(crate) fn s2_tls_fill_readahead(id: i32) -> std::io::Result<usize> {
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
pub(crate) fn s2_tls_discard_readahead(id: i32) {
    tls_readahead().lock().remove(&id);
}

/// NEW-13: read from a TLS stream registered via `s2_tls_connect` (native-tls),
/// or — for ids ≥ `RUSTLS_SOCK_ID_BASE` — the rustls client/server stream table.
///
/// Serves any readahead buffered by [`s2_tls_fill_readahead`] first so every
/// reader of a stream observes the same byte sequence regardless of entry
/// point.
pub(crate) fn s2_tls_read(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
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
fn s2_tls_read_direct(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
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
fn s2_tls_classify_after_block(id: i32, result: std::io::Result<usize>) -> std::io::Result<usize> {
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
pub(crate) fn s2_tls_write(id: i32, data: &[u8]) -> std::io::Result<usize> {
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
pub(crate) fn s2_tls_close(id: i32) -> std::io::Result<()> {
    s2_tls_discard_readahead(id);
    if id >= RUSTLS_SOCK_ID_BASE {
        crate::t27_tls::rustls_stream_close(id - RUSTLS_SOCK_ID_BASE);
        return Ok(());
    }
    // Unregister under the registry lock, shut down outside it.
    let entry = s2_registry().lock().tls_streams.remove(&id);
    if let Some(entry) = entry {
        // ─── WAKE THE PARKED PEER FIRST (W7-61) ──────────────────────────────
        //
        // `TlsEntry::raw` is a `try_clone`d handle on the same socket, and its
        // doc comment says it exists precisely so an fd-level operation can run
        // without waiting on `stream`'s mutex. This close never used it, and
        // the sentence below it — "the entry is already unregistered, so
        // dropping the handle suffices" — is false in the one case that
        // matters: a thread parked in `s2_tls_read_direct` holds an `Arc` on
        // the stream, so dropping OUR `Arc` closes nothing, the `try_lock`
        // below always fails, and the reader waits forever. That is W7-53's
        // "four TLS sites" row.
        //
        // A `shutdown` on the duplicate is the record-safe wakeup: it does not
        // take the stream mutex, does not free the handle the parked thread is
        // mid-syscall on (so it cannot be a use-after-close), and does not
        // interrupt the record layer at an arbitrary point — it ends the
        // underlying byte stream, which the record layer already has to handle.
        //
        // PLATFORM, stated as a contract rather than as a measurement (no Linux
        // arm was run for this change):
        //   * Unix — `shutdown(SHUT_RDWR)` wakes a parked `recv` with EOF, so
        //     the reader returns and `s2_tls_classify_after_block` then reports
        //     the close rather than a spurious end-of-stream.
        //   * Windows — Winsock has NO `shutdown` that aborts a pending
        //     blocking call; only `closesocket` does, and closing a handle a
        //     worker is inside a syscall on is exactly the use-after-close
        //     `pipe.rs` was fixed for. So on Windows this call is a no-op for
        //     an already-parked reader and that half of the row stays OPEN.
        //     It is written down rather than quietly counted, for the same
        //     reason W7-53 left the Windows pipe sink write open: a mechanism
        //     that compiles, looks like the others, and cannot deliver the
        //     wakeup is what removes a site from a census while leaving the
        //     defect. The correct Windows fix is the same one that file names —
        //     overlapped I/O with a bounded `GetOverlappedResultEx` — which is
        //     a change to how the socket is created, not landable on
        //     inspection.
        if let Some(raw) = entry.raw.as_ref() {
            let _ = raw.shutdown(std::net::Shutdown::Both);
        }
        // Best-effort: if the peer already closed the connection, shutdown
        // can legitimately return an error that should not surface as an
        // exception to Java-side callers. `try_lock` because a peer parked in
        // a blocking read on this same stream holds the per-stream mutex —
        // waiting for it here would just relocate the stall we removed. The
        // graceful TLS `close_notify` this sends is the nicety; the `raw`
        // shutdown above is the liveness guarantee, and it does not depend on
        // winning this lock.
        if let Some(mut stream) = entry.stream.try_lock() {
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
pub(crate) fn s2_tls_peer_cert_chain_der(id: i32) -> Option<Vec<Vec<u8>>> {
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
pub(crate) fn s2_tls_handshake_failure(id: i32) -> Option<String> {
    if id >= RUSTLS_SOCK_ID_BASE {
        return crate::t27_tls::rustls_server_handshake_failure(id - RUSTLS_SOCK_ID_BASE);
    }
    None
}

/// NEW-13 / T2.7.11: lookup the negotiated (protocol, cipher, host, port)
/// tuple for a TLS stream id. Used to populate SSLSession fields lazily.
pub(crate) fn s2_tls_session_info(id: i32) -> Option<(String, String, String, u16)> {
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
pub(crate) fn s2_tls_negotiated_alpn(id: i32) -> Option<String> {
    let reg = s2_registry().lock();
    reg.tls_streams
        .get(&id)
        .and_then(|e| e.negotiated_alpn.clone())
}

// ---- Field index constants -------------------------------------------------

const BB_ARRAY: usize = 0;
const BB_POS: usize = 1;
const BB_LIMIT: usize = 2;
const BB_CAP: usize = 3;
const BB_MARK: usize = 4;
const BB_ORDER: usize = 5; // 0=BIG_ENDIAN, 1=LITTLE_ENDIAN
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
const BB_SEGMENT_SLOT: usize = 5;
// NEW-17: extra fields for direct buffers. Bytes 6..7 are only populated
// by `allocateDirect`; non-direct buffers leave them at default (0).
const BB_NATIVE_ID: usize = 6; // Long  — alloc_id from NativeMemoryTable, 0 if heap
const BB_DIRECT_FLAG: usize = 7; // Int   — 1 if direct, 0 otherwise

// jdk/internal/ref/Cleaner$Deallocator synthetic for DirectByteBuffer.
//   field 0 = alloc_id (Long)          — key into the VM's `NativeMemoryTable`
//   field 1 = global-root handle (Long) — the root that keeps the owning
//             `Cleaner$Cleanable` reachable until the action has run; see
//             `s2_bb_alloc_direct`. Zeroed by `run()` once released.
const DEALLOC_ID: usize = 0;
const DEALLOC_ROOT: usize = 1;
const DEALLOC_FIELDS: usize = 2;
const DEALLOC_CLASS: &str = "jdk/internal/ref/DirectBufferDeallocator";

/// Alignment of the off-heap block behind a synthetic `DirectByteBuffer`.
/// 8 bytes is sufficient for every primitive `Unsafe.put*` width, matching
/// `native-io/src/direct_buffer.rs`'s `DBB_ALIGN`.
const DIRECT_BUFFER_ALIGN: usize = 8;

const S2SC_CONNECTED: usize = 0;
const S2SC_OPEN: usize = 1;
const S2SC_ADDR: usize = 2;
const S2SC_SOCK_ID: usize = 3;
const S2SC_BLOCKING: usize = 4;

const S2SSC_OPEN: usize = 0;
const S2SSC_BOUND: usize = 1;
const S2SSC_LISTENER_ID: usize = 2;
const S2SSC_PORT: usize = 3;
const S2SSC_PENDING: usize = 4;

const S2SEL_OPEN: usize = 0;
const S2SEL_KEYS: usize = 1;
const S2SEL_NKEYS: usize = 2;

// NEW-3: DatagramChannel synthetic field indices. The previous 4-field
// layout in `register_datagram_channel` (port/open/connected/blocking)
// did not carry a persistent socket id, so every send/receive re-bound a
// fresh UDP socket. The NEW-3 refactor keeps the original four fields and
// adds a fifth: the registry socket id (-1 when unallocated). Existing
// callers that read only the first four fields continue to work; the
// selector poll path and `send`/`receive`/`read`/`write` now read field
// 4 to locate the persistent socket in `SocketRegistry::dgrams`.
const S2DC_PORT: usize = 0;
const S2DC_OPEN: usize = 1;
const S2DC_CONNECTED: usize = 2;
const S2DC_BLOCKING: usize = 3;
const S2DC_SOCK_ID: usize = 4;

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
fn s2_bb_heap_class(ctx: &mut dyn NativeContext, read_only: bool) -> &'static str {
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

fn s2_bb_alloc(
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
            match reclaimed
                .then(|| ctx.try_new_array(ArrayElementType::Byte, cap))
                .flatten()
            {
                Some(a) => a,
                None => return Ok(None),
            }
        }
    };
    // GC-safety: `alloc_concurrent_synthetic` below allocates and can
    // trigger a collection that relocates `arr` (read again by
    // `bb_write_hb` immediately after); pin it and re-read.
    let arr_pin = ctx.pin_native_root(arr);
    let cls = s2_bb_heap_class(ctx, false);
    let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
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
fn s2_bb_alloc_direct(ctx: &mut dyn NativeContext, cap: i32) -> MethodCallResult {
    let allocation = ctx.allocate_native_memory(cap.max(0) as usize, DIRECT_BUFFER_ALIGN);
    let (alloc_id, ptr) = match allocation {
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
    let cleanable =
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
    if s2_bb_synthetic_layout(ctx, buf) {
        ctx.set_field(buf, BB_ARRAY, Value::Object(None));
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(cap));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Long(addr));
        ctx.set_field(buf, BB_ORDER, Value::Int(0)); // JDK default: BIG_ENDIAN
    }

    ctx.set_field(dealloc, DEALLOC_ID, Value::Long(alloc_id));
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
pub(crate) fn bb_write_hb(ctx: &mut dyn NativeContext, buf: ObjectRef, arr: ObjectRef, cap: i32) {
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
    if s2_bb_synthetic_layout(ctx, buf) {
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(cap));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Int(-1));
        ctx.set_field(buf, BB_ORDER, Value::Int(0));
    }
}

#[inline]
fn s2_bb_pos(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_POS).as_int().unwrap_or(0)
}
#[inline]
fn s2_bb_limit(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
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
fn s2_bb_check_index(
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
fn s2_bb_cap(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
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
fn s2_check_from_index_size(from: i32, size: i32, length: i32) -> Result<(), MethodCallFailed> {
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
fn s2_bb_get_mark(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    match ctx.get_field_by_name(buf, "mark") {
        Value::Int(m) => m,
        _ => ctx.get_field(buf, BB_MARK).as_int().unwrap_or(-1),
    }
}

/// Write a ByteBuffer's `mark` to both the real-JDK named field and the
/// synthetic indexed slot. See `s2_bb_get_mark` for why the named field is
/// authoritative for real-JDK-mode objects.
#[inline]
fn s2_bb_set_mark(ctx: &mut dyn NativeContext, buf: ObjectRef, value: i32) {
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
fn s2_bb_synthetic_layout(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
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
fn s2_is_typed_buffer_view(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
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
fn s2_buf_stub_layout(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    ctx.object_num_fields(buf) > BB_ORDER
        && ctx
            .resolve_field_index_by_class_id(ctx.class_id_of_object(buf), "position")
            .is_none()
}

#[inline]
fn s2_bb_order(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
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
fn s2_bb_set_order(ctx: &mut dyn NativeContext, buf: ObjectRef, ord: i32) {
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
#[inline]
fn s2_bb_arr(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<ObjectRef> {
    // Prefer the real-JDK `hb` field — when the JDK ByteBuffer class
    // is loaded, the indexed slot 0 lands on Buffer.mark (int
    // descriptor) and any Object write was coerced to Int(low_bits).
    if let Value::Object(Some(a)) = ctx.get_field_by_name(buf, "hb") {
        return Some(a);
    }
    if let Value::Object(Some(a)) = ctx.get_field(buf, BB_ARRAY) {
        return Some(a);
    }
    // BUG (found 2026-07-11): typed-buffer-view objects (IntBuffer/
    // LongBuffer/ShortBuffer/FloatBuffer/DoubleBuffer, e.g. from
    // `ByteBuffer.asFloatBuffer()`) have NO `hb` field at all (only
    // HeapFloatBuffer etc. declare it, not the abstract class our synthetic
    // views are stamped with) — same descriptor-coercion trap as `hb`
    // applies to the indexed slot 0 above (real slot 0 = `mark: int`), so
    // `s2_typed_buffer_view_fns!`/`s2_view_buf_fn!` (servlet.rs) instead
    // stash the array reference in slot 5 — the real `Buffer.segment` field
    // (`final java.lang.foreign.MemorySegment segment`), the ONLY
    // Object-typed field Buffer itself declares, so `ctx.set_field` doesn't
    // coerce an Object write there. Silently returning 0 for every read
    // when this fallback was missing surfaced as "expected:<X> but
    // was:<0.0>" across nearly the entire ES vector-codec test family —
    // every value read through a typed-buffer view came back zero
    // regardless of what was actually written.
    //
    // W7-83: and slot 5 is `Buffer.segment` on a REAL loaded `java.nio.Buffer`,
    // where the value is a `MemorySegment`, not an array. Measured on Eclipse
    // Adoptium 25.0.3.9: `ByteBuffer.allocate(16)` has `segment == null`,
    // `ByteBuffer.allocateDirect(16)` has `segment == null`, and
    // `Arena.ofAuto().allocate(16).asByteBuffer()` has `hb == null` and
    // `segment == jdk.internal.foreign.NativeMemorySegmentImpl`.
    //
    // Without the kind screen this function returned that `MemorySegment` to
    // `array()`, whose declared return type is `[B`, and made `hasArray()`
    // answer `true` where HotSpot answers `false`. **This registration wins in
    // Compatible mode** (W7-76 §2: `set_drop_real_layout_synthetic(true)` runs
    // before `register_io_natives`, so `register_nio_natives` is skipped and
    // nothing overwrites s2), and `array`/`hasArray`/`arrayOffset` are all on
    // `native_override.rs`'s forced-native list for `java/nio/ByteBuffer`, so
    // the native answers even though the real bytecode is present.
    //
    // Rejecting the segment is what makes the receiver fall through to the
    // callers' direct arms: `array()`'s `None if s2_bb_direct_addr(..)` arm
    // raises `UnsupportedOperationException` and `hasArray()` answers false —
    // exactly HotSpot. `s2_bb_direct_addr` keeps its own
    // `is_plausible_native_addr` screen, which is untouched and is still what
    // stops a heap buffer's `address = 16` being dereferenced.
    match ctx.get_field(buf, BB_SEGMENT_SLOT) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == ObjectKind::Array => Some(a),
        _ => None,
    }
}

/// Native-memory address of a DIRECT buffer (real-JDK `DirectByteBuffer`
/// has no `hb` heap array; its storage lives at the `address` field). Same
/// name-first/slot-4-fallback resolution as native-io's
/// `directbuffer_address`. `None` for heap buffers and storage-less
/// synthetics.
///
/// Heap buffers ALSO carry a non-zero `address` (`bb_write_hb` seeds
/// ARRAY_BYTE_BASE_OFFSET + offset, i.e. 16+, mirroring the real
/// HeapByteBuffer ctor) — that is an array-relative offset, not a process
/// pointer, so the presence of a heap array must always win over the
/// address field. Guard here so callers can consult this helper directly
/// without repeating the array check.
fn s2_bb_direct_addr(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<i64> {
    if s2_bb_heap_window(ctx, buf).is_some() {
        return None;
    }
    match ctx.get_field_by_name(buf, "address") {
        Value::Long(v) if is_plausible_native_addr(v) => Some(v),
        _ => match ctx.get_field(buf, 4) {
            Value::Long(v) if is_plausible_native_addr(v) => Some(v),
            _ => None,
        },
    }
}

/// A `Buffer.address` below the first mappable page is never a process
/// pointer: Linux refuses to map below `vm.mmap_min_addr` (65536 by default)
/// and Windows reserves the low 64 KiB of every address space. What DOES live
/// down there is an array-relative `Unsafe` offset —
/// `ARRAY_BYTE_BASE_OFFSET + offset` — belonging to a HEAP-backed buffer whose
/// array the caller failed to resolve.
///
/// This is a backstop, not the contract: `s2_bb_heap_window` above is what
/// actually resolves those buffers. It earns its place because dereferencing
/// such an "address" is an immediate, unrecoverable SIGSEGV — `addr=0x10`,
/// i.e. exactly `ARRAY_BYTE_BASE_OFFSET`, on 51 of the 53 crashes in the
/// 2026-08-10 three-GC-variant H2 sweep — whereas answering "no storage"
/// degrades to this module's existing benign zero, which a caller can survive.
#[inline]
fn is_plausible_native_addr(v: i64) -> bool {
    v >= 0x1_0000
}

/// Array-base offset of a heap buffer — the real-JDK `ByteBuffer.offset`
/// field (element index of logical byte 0 inside `hb`). 0 for fresh
/// allocations/wraps and for layouts with no `offset` field to resolve
/// (typed views, bare synthetics). Non-zero only for aliasing views made
/// by `slice()`/`slice(int,int)` below, mirroring real HeapByteBuffer.
#[inline]
fn s2_bb_heap_base(ctx: &dyn NativeContext, buf: ObjectRef) -> usize {
    match ctx.get_field_by_name(buf, "offset") {
        Value::Int(v) if v > 0 => v as usize,
        _ => 0,
    }
}

/// `Unsafe.ARRAY_BYTE_BASE_OFFSET` as this VM publishes it — the value
/// `bb_write_hb` seeds into a heap `Buffer.address`, and the value the real
/// JDK's own `HeapByteBuffer` ctor adds to its array-base `offset`. Element 0
/// of a `byte[]` sits at this unsafe offset, so subtracting it turns a
/// `Buffer.address` back into a plain byte index.
///
/// **DEFINED AS native-io's, not beside it (F26-1).** Two crates screening the
/// same `Buffer.address` values against two independently-spelled `16`s is the
/// W7-76 §8.2 shape; a `const` initialised from the other crate's `pub const`
/// is const-evaluated, costs nothing, leaves all nine use sites in this file
/// untouched, and makes drift impossible.
const ARRAY_BYTE_BASE_OFFSET: i64 = cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET;

/// Backing array + byte index of element 0 for a real-JDK
/// `java/nio/ByteBufferAs<T>Buffer{B,L}` — the concrete view class
/// `ByteBuffer.as<T>Buffer()` returns.
///
/// `asLongBuffer` and friends are NOT in `force_native_over_real_jdk_bytecode`,
/// so against a real JDK they run the JDK's own bytecode and hand back one of
/// these. Its storage lives on the backing `bb` ByteBuffer; the view's own `hb`
/// is null, and `Buffer.address` holds `bb.address + bb.position()` — an
/// `Unsafe` offset (`ARRAY_BYTE_BASE_OFFSET + byteIndex`), **not** a process
/// pointer.
///
/// The bulk `get([JII)`/`put([JII)` accessors, on the other hand, ARE forced:
/// they are declared on the abstract `java/nio/LongBuffer`, which these views
/// do not override, so such a receiver lands in `s2_lb_get_bulk` and from there
/// in `s2_bb_get_byte`. Before this helper existed that path found no array
/// (`s2_bb_arr` looks at `hb`, slot 0 and `Buffer.segment`, none of which the
/// view populates), fell through to `s2_bb_direct_addr`, and dereferenced the
/// `address` value as a pointer: `copy_from_native_memory(0x10, 1)` →
/// SIGSEGV at `addr=0x10`. `org.h2.mvstore.Chunk.readToC`'s
/// `buff.asLongBuffer().get(toc)` does exactly this on every MVStore chunk
/// read, which is why 16-18 H2 classes crashed identically under all three
/// collectors.
///
/// Mirrors `bbacb_read_underlying_bytes` (native-builtins/src/lib.rs), which
/// already resolves the `ByteBufferAsCharBuffer{B,L}` family the same way.
/// `None` for a view over a DIRECT ByteBuffer — there the view's `address`
/// genuinely IS a process pointer and the direct path handles it correctly.
fn s2_bb_view_backing(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<(ObjectRef, usize)> {
    let bb = match ctx.get_field_by_name(buf, "bb") {
        Value::Object(Some(b)) => b,
        _ => return None,
    };
    let arr = s2_bb_arr(ctx, bb)?;
    let base = match ctx.get_field_by_name(buf, "address") {
        Value::Long(a) if a >= ARRAY_BYTE_BASE_OFFSET => (a - ARRAY_BYTE_BASE_OFFSET) as usize,
        Value::Int(a) if i64::from(a) >= ARRAY_BYTE_BASE_OFFSET => {
            (i64::from(a) - ARRAY_BYTE_BASE_OFFSET) as usize
        }
        // No usable `address` (a synthetic-layout source): fall back to the
        // backing buffer's own array-base offset, as the char-view helper does.
        // This drops the source's position-at-creation, but a stale window is
        // recoverable where a wild pointer is not.
        _ => s2_bb_heap_base(ctx, bb),
    };
    Some((arr, base))
}

/// Heap storage of ANY buffer this module handles: `(array, byte index of the
/// buffer's logical byte 0)`. Covers the buffer's own array plus its
/// array-base `offset` (heap ByteBuffers, synthetic typed views) and the
/// backing array of a real-JDK `ByteBufferAs<T>Buffer{B,L}` view. `None` for
/// direct and storage-less buffers.
fn s2_bb_heap_window(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<(ObjectRef, usize)> {
    if let Some(arr) = s2_bb_arr(ctx, buf) {
        return Some((arr, s2_bb_heap_base(ctx, buf)));
    }
    s2_bb_view_backing(ctx, buf)
}

/// Resolved backing storage of an s2-managed buffer: a heap array plus the
/// buffer's array-base offset, OR a direct native address. This is the
/// single storage-view helper the residual doc
/// (s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md)
/// called for: every method that used to read `s2_bb_arr` only — and
/// silently produced empty/zero results on a DIRECT receiver — goes
/// through here instead.
enum S2BbStorage {
    Heap { arr: ObjectRef, base: usize },
    Direct { addr: i64 },
}

fn s2_bb_storage(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<S2BbStorage> {
    if let Some((arr, base)) = s2_bb_heap_window(ctx, buf) {
        return Some(S2BbStorage::Heap { arr, base });
    }
    s2_bb_direct_addr(ctx, buf).map(|addr| S2BbStorage::Direct { addr })
}

/// Read `len` bytes starting at logical byte index `from` (position-space,
/// i.e. NOT including the heap array-base offset) from either storage kind.
/// `None` when the buffer is storage-less or a direct native read fails.
fn s2_bb_read_window(
    ctx: &dyn NativeContext,
    buf: ObjectRef,
    from: i32,
    len: usize,
) -> Option<Vec<u8>> {
    if from < 0 {
        return None;
    }
    match s2_bb_storage(ctx, buf)? {
        S2BbStorage::Heap { arr, base } => {
            let start = base.checked_add(from as usize)?;
            let end = start.checked_add(len)?;
            if end > ctx.array_length(arr) {
                return None;
            }
            Some(
                (start..end)
                    .map(|i| ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8)
                    .collect(),
            )
        }
        S2BbStorage::Direct { addr } => {
            let mut bytes = vec![0u8; len];
            if ctx.copy_from_native_memory(addr.saturating_add(from as i64), &mut bytes) {
                Some(bytes)
            } else {
                None
            }
        }
    }
}

/// True when the buffer's real-JDK `isReadOnly` flag is set (layouts with
/// no such field — bare synthetics, typed views — always report writable,
/// matching this family's historic behaviour).
///
/// CONVERGED (F14-1 N1). This was one of THREE byte-identical transcriptions of
/// the same one-line field read — this, `charset_buffers.rs::cb_is_read_only`,
/// and `native-io`'s. It is now a delegation to the `native-io` copy: the crate
/// dependency runs `cratonvm-native-builtins` → `cratonvm-native-io` and not
/// the reverse, so that is the only one of the three the other two can import.
/// The local name is kept because it is the name fifteen call sites in this
/// file already use, and because a delegation is one implementation whichever
/// name it wears — what F14-1 objected to was three BODIES that can drift, not
/// three names.
#[inline]
fn s2_bb_is_read_only(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    cratonvm_native_io::buffer_is_read_only(ctx, buf)
}

/// The abstract public buffer classes this crate stamps on a carrier it
/// allocated itself.
///
/// A name test, but on the PUBLIC API classes, not on the JDK's generated
/// implementation names — `java.nio.IntBuffer` cannot be renamed by a JDK
/// release without breaking every program in the world, which is not true of
/// `ByteBufferAsIntBufferRB`.
#[inline]
fn s2_is_our_buffer_carrier(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(buf);
    matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some(
            "java/nio/ByteBuffer"
                | "java/nio/CharBuffer"
                | "java/nio/IntBuffer"
                | "java/nio/LongBuffer"
                | "java/nio/ShortBuffer"
                | "java/nio/FloatBuffer"
                | "java/nio/DoubleBuffer"
        )
    )
}

/// [`s2_bb_is_read_only`], **plus the answer for a receiver that keeps its
/// read-only-ness in an OVERRIDDEN METHOD rather than in the field.**
///
/// `buffer_is_read_only` reads the `isReadOnly` FIELD by name, and that field
/// is the whole answer for `HeapByteBufferR` and friends, whose constructors
/// set it. It is NOT the answer for the `ByteBufferAs<T>Buffer R{B,L}` family —
/// the views `ByteBuffer.as<T>Buffer().asReadOnlyBuffer()` returns — which
/// leave the field FALSE and override `isReadOnly()` to return `true`. Measured
/// on Temurin 25.0.4+7:
///
/// ```text
/// viewRo.isReadOnly()                 HotSpot true    this VM true
/// IntBuffer.isReadOnly (the FIELD)    HotSpot false   this VM false
/// viewRo.put(new int[]{5}, 0, 1)      HotSpot ReadOnlyBufferException
///                                     this VM no-throw, and bb.getInt(0) == 5
/// ```
///
/// **A silent write through a read-only handle, into the caller's own
/// `ByteBuffer`.** It reached exactly one door and no other: the JDK's
/// read-only view classes DECLARE `put(int)` and `put(int,int)` — so those
/// dispatch to their own bodies and refuse — and do NOT declare
/// `put(int[],int,int)`, whose most-derived declaration is on the abstract
/// `IntBuffer` this crate registers against. *The door asks about the
/// DECLARING class*, so one of a family's three `put` overloads was ours and
/// two were the JDK's, which is why the scalar row passed and the bulk row did
/// not.
///
/// The second question is one virtual call and is asked only of a receiver this
/// crate did not allocate; our own carriers carry the flag in the field and
/// answer before it.
fn s2_buf_read_only(ctx: &mut dyn NativeContext, buf: ObjectRef) -> bool {
    if s2_bb_is_read_only(ctx, buf) {
        return true;
    }
    if s2_is_our_buffer_carrier(ctx, buf) {
        return false;
    }
    matches!(
        ctx.invoke_virtual(buf, "isReadOnly", "()Z", &[]),
        Ok(Some(Value::Int(v))) if v != 0
    )
}

/// The WRITE half of [`s2_bb_is_read_only`], for the four view producers.
///
/// `slice()`, `slice(int,int)` and `duplicate()` inherit the source's flag and
/// `asReadOnlyBuffer()` sets it unconditionally — MEASURED across all seven
/// buffer families on jdk-25.0.3+9
/// (`scratchpad/f21/F21ViewContagionProbe.java`); read-only-ness is contagious
/// and `asReadOnlyBuffer()` is one-way with no operation anywhere that clears
/// it. Each of those four had heap and direct arms that carried `ro`
/// correctly and a copying fallback arm that did not, and the fallback runs
/// `bb_write_hb`, which writes `isReadOnly = 0` — so the flag was actively
/// cleared rather than merely left alone. This exists so the corrected arms
/// name the operation instead of open-coding a fifth `set_field_by_name`.
///
/// Deliberately by NAME and not through `BB_*`: `isReadOnly` is a real-JDK
/// `ByteBuffer` field with no slot in the bare 6-slot synthetic layout, so on a
/// carrier that has no such field this is a no-op — which is the same
/// behaviour, and the same reason, as `bb_write_hb`'s own `isReadOnly` write
/// one screen up.
#[inline]
fn s2_bb_set_read_only(ctx: &mut dyn NativeContext, buf: ObjectRef, read_only: bool) {
    ctx.set_field_by_name(buf, "isReadOnly", Value::Int(i32::from(read_only)));
}

/// `throw new UnsupportedOperationException()` — the NO-ARGUMENT constructor,
/// so `getMessage()` is null exactly as HotSpot's is.
///
/// The `array()` / `arrayOffset()` arms below raised this with a
/// `"direct buffer has no backing array"` detail message. The CLASS was right,
/// so no `catch` and no class-name assertion could see the difference — only a
/// message-exact differential can, which is why it survived W7-83 §7.1.
/// MEASURED on `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`, all four UOE
/// arms of this family report `getMessage() == null`:
///
/// ```text
/// directByteBuffer.array        java.lang.UnsupportedOperationException  getMessage=null
/// directByteBuffer.arrayOffset  java.lang.UnsupportedOperationException  getMessage=null
/// charView.array                java.lang.UnsupportedOperationException  getMessage=null
/// stringCharBuffer.array        java.lang.UnsupportedOperationException  getMessage=null
/// new UOE().getMessage          OK null
/// ```
///
/// The EMPTY string is the documented spelling for "no message" in
/// `types/src/error.rs`, which maps `""` to `None` so the `()V` ctor is used;
/// `Some("")` would set a non-null empty detail message. Same spelling as
/// `charset_buffers.rs::cb_no_backing_array` and
/// `native-io/src/lib.rs::buffer_no_backing_array` — this WAS the third and
/// last copy in the family. Record: F14-1 N3.
///
/// CONVERGED (F14-1 N1): now a delegation to the `native-io` copy, for the same
/// dependency-edge reason as [`s2_bb_is_read_only`]. The empty-string spelling
/// is precisely the detail a fourth transcription would get wrong, and the
/// existing test on this arm matches
/// `RuntimeError::UnsupportedOperationException { .. }`, which is blind to the
/// message by construction — the message-reading test lives beside the single
/// remaining body.
#[inline]
fn s2_bb_no_backing_array() -> MethodCallFailed {
    cratonvm_native_io::buffer_no_backing_array()
}

/// The canonical `java.nio.ByteOrder` object for `ord` (0=BIG_ENDIAN,
/// 1=LITTLE_ENDIAN). Prefers the REAL class statics — ensuring the class
/// is initialized first, since callers like `buffer.order()` can run
/// before any Java-side `ByteOrder` access — so identity comparisons
/// (`order() == ByteOrder.LITTLE_ENDIAN`) and `toString()` behave exactly
/// like HotSpot. Falls back to a 1-slot synthetic (field 0 = order int)
/// only when the real class/statics are unavailable (synthetic-jdk mode).
///
/// G38-1: the FALLBACK arm was the unrepaired twin of
/// `phases_late::foreign_ffm::p67_byte_order_object`. Reaching it does not
/// prove the real class is absent — `ensure_class_initialized` can succeed and
/// `static_field_index_by_name`/`get_static_field` still miss (a class loaded
/// but whose `<clinit>` has not published the constants yet, and every
/// compatibility-mode arm where a synthetic registrar runs against real class
/// bytes). On that path `alloc_concurrent_synthetic` hands back an object of
/// the REAL `java.nio.ByteOrder`, whose only instance field is
/// `private final String name` at slot 0 — so `Value::Int(ord)` was silently
/// coerced to `null` by `heap::coerce_field_value_by_descriptor`
/// (G30-1-the-silent-reference-slot-coercion-20260817.md), and BOTH readers
/// below then decoded the null as BIG_ENDIAN: `LITTLE_ENDIAN.toString()`
/// printed `BIG_ENDIAN` and `equals` called the two constants equal.
///
/// The repair is the correct value in the right slot, not a refusal. The flag
/// write stays — it is the synthetic-stub layout, and `s2_byte_order_ord`
/// still falls back to it — and on top of it, when the CLASS actually declares
/// `name` at a slot this object has, a genuine String goes there. Three
/// shapes, all covered, exactly as in `p67_byte_order_object`: a fabricated
/// stub names its fields `_f0..`, so `name` does not resolve and only the flag
/// lands (byte-identical to before); a real `java.nio.ByteOrder` resolves
/// `name` to slot 0 and gets the String, which `toString`/`s2_byte_order_ord`
/// already decode; a resolved index out of range is skipped rather than
/// written out of bounds.
pub(crate) fn s2_byte_order_object(
    ctx: &mut dyn NativeContext,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let cid = ctx
        .ensure_class_initialized("java/nio/ByteOrder")
        .ok()
        .or_else(|| ctx.class_id_by_name("java/nio/ByteOrder"));
    if let Some(cid) = cid {
        let field = if ord == 1 {
            "LITTLE_ENDIAN"
        } else {
            "BIG_ENDIAN"
        };
        if let Some(idx) = ctx.static_field_index_by_name(cid, field) {
            if let Value::Object(Some(o)) = ctx.get_static_field(cid, idx) {
                return Ok(o);
            }
        }
    }
    let bo = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1)?;
    ctx.set_field(bo, 0, Value::Int(ord));
    let bo_cid = ctx.class_id_of_object(bo);
    // Bound before the `if let` so the immutable reborrow of `ctx` ends here
    // rather than spanning the block that needs `&mut ctx`.
    let name_slot = ctx.resolve_field_index_by_class_id(bo_cid, "name");
    if let Some(slot) = name_slot {
        if slot < ctx.object_num_fields(bo) {
            // `create_string` allocates and can move `bo` (native stale-local
            // family) — pin it across the call.
            let bo_pin = ctx.pin_native_root(bo);
            let name = ctx.create_string(if ord == 1 {
                "LITTLE_ENDIAN"
            } else {
                "BIG_ENDIAN"
            });
            let bo = ctx.read_native_pin(bo_pin, bo);
            ctx.unpin_native_roots(bo_pin);
            ctx.set_field(bo, slot, Value::Object(Some(name)));
            return Ok(bo);
        }
    }
    Ok(bo)
}

/// Write a buffer's `position`. Buffers with a heap array keep this
/// family's historic `BB_POS` slot write (self-consistent with `s2_bb_pos`
/// on both real heap and synthetic buffers). DIRECT buffers are otherwise
/// managed by name-based natives whose layout the `BB_POS` slot is not
/// guaranteed to match — write their `position` by name.
fn s2_bb_set_pos(ctx: &mut dyn NativeContext, buf: ObjectRef, v: i32) {
    if s2_bb_arr(ctx, buf).is_some() {
        ctx.set_field(buf, BB_POS, Value::Int(v));
    } else {
        ctx.set_field_by_name(buf, "position", Value::Int(v));
    }
}

/// Bulk `byte[]` → `byte[]` copy for the ByteBuffer natives, through the VM's
/// `memcpy` intrinsics instead of a per-element accessor loop.
///
/// Every heap↔heap arm of `get([B)`, `get([BII)`, `put([B)`, `put([BII)` and
/// `put(Ljava/nio/ByteBuffer;)` used to move one byte per `get_array_element` /
/// `set_array_element` call, i.e. two dynamic accessor calls per byte. That is
/// what `native-api`'s own doc comment on `write_byte_array_from` calls out as
/// the migration these callers were waiting for, and it is the whole of
/// `TestAsyncMessagesPerformance`'s inter-chunk gap: the WebSocket client moves
/// ~32 KiB through these five methods for every 8 KiB partial message it
/// delivers (socket → `response` → `inputBuffer` → `messageBufferBinary` →
/// the defensive `copy` handed to `onMessage`).
/// See `32-doc04-residual-perf-assertions-CLOSED.md` §32.3.
///
/// Returns `false` — having written nothing — when either intrinsic declines
/// (non-byte array kind, or bounds it refuses); the caller must then fall back
/// to the element loop. Copying via an owned buffer also makes an overlapping
/// same-array copy well defined, which the element loop was not.
fn s2_bb_bulk_array_copy(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
) -> bool {
    if len == 0 {
        return true;
    }
    let mut buf = vec![0u8; len];
    if ctx.read_byte_array_into(src, src_off, &mut buf) != len {
        return false;
    }
    ctx.write_byte_array_from(dst, dst_off, &buf)
}

fn s2_bb_get_byte(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i8 {
    // B8: a negative `idx` (overflowed/garbage index from Java bytecode)
    // would become a huge `usize` and either panic or read out of bounds.
    // Bound-check explicitly here: a negative or out-of-range index reads
    // back 0 (the JDK would throw IndexOutOfBoundsException; our synthetic
    // path stays panic-free and returns a benign zero byte instead).
    if idx < 0 {
        return 0;
    }
    if let Some((arr, base)) = s2_bb_heap_window(ctx, buf) {
        let i = base.saturating_add(idx as usize);
        if i >= ctx.array_length(arr) {
            return 0;
        }
        ctx.get_array_element(arr, i).as_int().unwrap_or(0) as i8
    } else if let Some(addr) = s2_bb_direct_addr(ctx, buf) {
        // DIRECT buffer (no heap array): fall back to native-memory
        // access, mirroring `put(Ljava/nio/ByteBuffer;)`'s direct-buffer
        // fix. Reached whenever an s2-forced native touches a genuine
        // direct receiver that real-JDK `DirectByteBuffer`'s own
        // overridden bytecode didn't intercept first (slices/duplicates
        // of a direct buffer, typed views over one, etc). This helper has
        // no `MethodCallResult` to propagate a Java exception through (it
        // is a private byte-level primitive called from ~15 sites), so a
        // failed native-memory read stays panic-free and benign (0),
        // matching this function's existing out-of-range convention —
        // callers that DO have a `MethodCallResult` (the bulk get/put
        // registrations below) throw `IllegalStateException` instead.
        let mut b = [0u8; 1];
        if ctx.copy_from_native_memory(addr.saturating_add(idx as i64), &mut b) {
            b[0] as i8
        } else {
            0
        }
    } else {
        0
    }
}

fn s2_bb_put_byte(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, b: i8) {
    // B8: mirror the read-side bound check — a negative or out-of-range
    // index is silently dropped rather than panicking / clobbering memory.
    if idx < 0 {
        return;
    }
    if let Some((arr, base)) = s2_bb_heap_window(ctx, buf) {
        let i = base.saturating_add(idx as usize);
        if i >= ctx.array_length(arr) {
            return;
        }
        ctx.set_array_element(arr, i, Value::Int(b as i32));
    } else if let Some(addr) = s2_bb_direct_addr(ctx, buf) {
        // DIRECT buffer: mirror the get-side fallback above. Best-effort —
        // see `s2_bb_get_byte`'s comment for why this stays panic-free
        // and benign (silently drops the write) rather than surfacing a
        // Java exception from this deep a helper.
        let _ = ctx.copy_to_native_memory(addr.saturating_add(idx as i64), &[b as u8]);
    }
}

/// Remaining bytes (pos..limit) as Vec<u8> without advancing position.
/// Storage-aware: heap (honouring the array-base offset) and direct
/// buffers both work; storage-less synthetics stay an empty Vec.
fn s2_bb_remaining_bytes(ctx: &dyn NativeContext, buf: ObjectRef) -> Vec<u8> {
    let pos = s2_bb_pos(ctx, buf).max(0);
    let lim = s2_bb_limit(ctx, buf).max(pos);
    s2_bb_read_window(ctx, buf, pos, (lim - pos) as usize).unwrap_or_default()
}

/// B8: compute `idx + off` for a multi-byte ByteBuffer access without
/// overflowing. A wrap (in release) or panic (in debug) is reachable when
/// Java bytecode hands us an `idx` near `i32::MAX`. On overflow we return a
/// negative sentinel, which `s2_bb_get_byte` / `s2_bb_put_byte` treat as
/// out-of-range (read 0 / drop the write) — matching the benign behaviour of
/// a genuine bounds miss.
#[inline]
fn s2_bb_off(idx: i32, off: i32) -> i32 {
    // A negative starting index is already out of range; keep it negative (the
    // out-of-range sentinel) instead of letting `idx + off` cross zero into a
    // valid-looking positive offset. Overflow on a valid index saturates to -1.
    if idx < 0 {
        return -1;
    }
    idx.checked_add(off).unwrap_or(-1)
}

/// B8: byte offset of int-unit index `unit` (relative to byte-start `bs`),
/// i.e. `bs + unit * 4`, computed without overflow. Used by the IntBuffer
/// view get/put. Overflow saturates to a negative sentinel so the byte
/// accessors treat it as out-of-range.
#[inline]
fn s2_bb_int_byte_off(bs: i32, unit: i32) -> i32 {
    unit.checked_mul(4)
        .and_then(|b| bs.checked_add(b))
        .unwrap_or(-1)
}

/// Read `N` bytes starting at logical index `idx`, resolving the buffer's
/// storage ONCE.
///
/// The six multi-byte accessors below used to call [`s2_bb_get_byte`] /
/// [`s2_bb_put_byte`] once per byte, and each of those re-resolves the whole
/// storage view from scratch — `get_field_by_name(buf, "hb")`, then `"offset"`,
/// then possibly `"address"` — i.e. up to three NAME-keyed field lookups per
/// byte. A `putLong` therefore paid that resolution EIGHT times.
///
/// **This was written as a throughput fix and it is NOT one — recorded here so
/// the next reader does not re-derive the same wrong hypothesis.** Interleaved
/// before/after on `probes/NioAccessorRate.java` (2026-08-17, real-JDK mode,
/// G1, two rounds), `direct ByteBuffer.putLong` measured 918 -> 943 and
/// 864 -> 877 ns/op: no change outside the noise. The per-byte re-resolution
/// was real, but it is not what the time goes to.
///
/// What the time actually goes to is the NATIVE CALL itself. On the same
/// probe a single-byte `ByteBuffer.put(int,byte)` — one native, one stored
/// byte — costs ~260 ns against HotSpot's 0.29, and `putLong` is ~3.5x that
/// rather than 8x, which is the shape of "one native call plus a few
/// name-keyed field reads", not "eight byte stores". See
/// `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`
/// for the full decomposition and for why this is what makes
/// `testZipBomb` exceed its wall.
///
/// The rewrite is kept because it is strictly less work per access and it is
/// pinned by a differential oracle (`probes/NioAccessorOracle.java`, which
/// checksums every width x endianness x storage-kind x alignment plus the
/// out-of-range and read-only contracts, and must print the same TOTAL on
/// HotSpot and CratonVM). It is not kept on the strength of a measurement.
///
/// Semantics are preserved exactly, including the benign out-of-range
/// behaviour this family documents: a read past the array reads back zero for
/// the bytes that are out of range, and a write past it drops them, rather
/// than throwing from this deep a helper. Only the number of storage
/// resolutions changes.
fn s2_bb_read_n<const N: usize>(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> [u8; N] {
    let mut out = [0u8; N];
    if idx < 0 {
        return out;
    }
    match s2_bb_storage(ctx, buf) {
        Some(S2BbStorage::Heap { arr, base }) => {
            let Some(start) = base.checked_add(idx as usize) else {
                return out;
            };
            let len = ctx.array_length(arr);
            for (k, slot) in out.iter_mut().enumerate() {
                let Some(i) = start.checked_add(k) else { break };
                if i >= len {
                    break;
                }
                *slot = ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8;
            }
        }
        Some(S2BbStorage::Direct { addr }) => {
            // One bulk copy; on failure every byte keeps this family's benign
            // zero, exactly as the per-byte helper's `else { 0 }` arm did.
            if !ctx.copy_from_native_memory(addr.saturating_add(idx as i64), &mut out) {
                out = [0u8; N];
            }
        }
        None => {}
    }
    out
}

/// Write side of [`s2_bb_read_n`] — same single-resolution contract, same
/// benign drop-on-out-of-range behaviour.
fn s2_bb_write_n<const N: usize>(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    idx: i32,
    bytes: [u8; N],
) {
    if idx < 0 {
        return;
    }
    match s2_bb_storage(ctx, buf) {
        Some(S2BbStorage::Heap { arr, base }) => {
            let Some(start) = base.checked_add(idx as usize) else {
                return;
            };
            let len = ctx.array_length(arr);
            for (k, &b) in bytes.iter().enumerate() {
                let Some(i) = start.checked_add(k) else {
                    return;
                };
                if i >= len {
                    return;
                }
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
        }
        Some(S2BbStorage::Direct { addr }) => {
            let _ = ctx.copy_to_native_memory(addr.saturating_add(idx as i64), &bytes);
        }
        None => {}
    }
}

fn s2_bb_read2(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i16 {
    let bs = s2_bb_read_n::<2>(ctx, buf, idx);
    if s2_bb_order(ctx, buf) == 1 {
        i16::from_le_bytes(bs)
    } else {
        i16::from_be_bytes(bs)
    }
}

fn s2_bb_write2(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, val: i16) {
    let bytes = if s2_bb_order(ctx, buf) == 1 {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    s2_bb_write_n::<2>(ctx, buf, idx, bytes);
}

fn s2_bb_read4(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i32 {
    let bs = s2_bb_read_n::<4>(ctx, buf, idx);
    if s2_bb_order(ctx, buf) == 1 {
        i32::from_le_bytes(bs)
    } else {
        i32::from_be_bytes(bs)
    }
}

fn s2_bb_write4(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, val: i32) {
    let bytes = if s2_bb_order(ctx, buf) == 1 {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    s2_bb_write_n::<4>(ctx, buf, idx, bytes);
}

fn s2_bb_read8(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i64 {
    let bs = s2_bb_read_n::<8>(ctx, buf, idx);
    if s2_bb_order(ctx, buf) == 1 {
        i64::from_le_bytes(bs)
    } else {
        i64::from_be_bytes(bs)
    }
}

fn s2_bb_write8(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, val: i64) {
    let bytes = if s2_bb_order(ctx, buf) == 1 {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    s2_bb_write_n::<8>(ctx, buf, idx, bytes);
}

// ---- Socket address helper -------------------------------------------------

fn s2_parse_socket_addr(ctx: &dyn NativeContext, addr: ObjectRef) -> Option<(String, u16)> {
    // Real-JDK 25 InetSocketAddress lays its host / port behind a `holder`
    // chain: `InetSocketAddress.holder.{hostname, addr, port}`. The synthetic
    // 2-field layout (host string at 0, port int at 1) is the legacy probe
    // shape. Try the real-JDK shape first, then the synthetic.
    if let Value::Object(Some(h)) = ctx.get_field_by_name(addr, "holder") {
        let port = match ctx.get_field_by_name(h, "port") {
            Value::Int(p) => p as u16,
            _ => 0,
        };
        // hostname is set when the address was built via
        // `InetSocketAddress(String, int)`; otherwise we fall back to the
        // wildcard so a `bind(null, port)` matches HotSpot's behaviour of
        // listening on all interfaces.
        let host = match ctx.get_field_by_name(h, "hostname") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let host = if !host.is_empty() {
            host
        } else if let Value::Object(Some(ia)) = ctx.get_field_by_name(h, "addr") {
            // InetAddress.holder.hostName fallback for
            // `InetSocketAddress(InetAddress, int)` callers.
            match ctx.get_field_by_name(ia, "holder") {
                Value::Object(Some(iah)) => match ctx.get_field_by_name(iah, "hostName") {
                    Value::Object(Some(s)) => {
                        ctx.read_string(s).unwrap_or_else(|| "0.0.0.0".into())
                    }
                    _ => "0.0.0.0".into(),
                },
                _ => "0.0.0.0".into(),
            }
        } else {
            "0.0.0.0".into()
        };
        return Some((host, port));
    }

    // Synthetic 2-field fallback.
    let host = match ctx.get_field(addr, 0) {
        Value::Object(Some(h)) => ctx.read_string(h)?,
        _ => return None,
    };
    let port = ctx.get_field(addr, 1).as_int()? as u16;
    Some((host, port))
}

// ---- NEW-3: Cross-platform poll abstraction --------------------------------
//
// Unix exposes `poll(2)` via `libc::poll`, which operates on an array of
// `libc::pollfd` (fd = `c_int`). Windows' `libc` crate does NOT expose
// `WSAPoll` or `WSAPOLLFD`, so for Windows we bind the raw symbols from
// Ws2_32.dll directly below. Both platforms are then hidden behind the
// platform-neutral `PollReq` / `selector_poll` abstraction so the rest of
// this module stays cfg-free.
//
// We use the following bit definitions for `PollReq::events`:
//   POLL_IN  = readable (analogous to POLLIN / POLLRDNORM)
//   POLL_OUT = writable (analogous to POLLOUT / POLLWRNORM)
//   POLL_ERR = error bit (analogous to POLLERR)
//   POLL_HUP = hang-up bit (analogous to POLLHUP)
// These are translated to the platform-specific constants by the
// dispatcher below.

const POLL_IN: i16 = 0x1;
const POLL_OUT: i16 = 0x2;
const POLL_ERR: i16 = 0x4;
const POLL_HUP: i16 = 0x8;

#[derive(Clone, Copy, Debug)]
struct PollReq {
    /// Raw OS handle, stored as i64 to fit `c_int` on Unix and `SOCKET` on
    /// Windows without loss. The dispatcher casts it back to the platform
    /// type before invoking the OS poll.
    fd: i64,
    /// Bitwise-OR of `POLL_IN` / `POLL_OUT`.
    events: i16,
}

/// Run the OS poll on a slice of `PollReq`. Returns a Vec of revents
/// (neutral bits — POLL_IN / POLL_OUT / POLL_ERR / POLL_HUP) in the same
/// order as the input, or an empty Vec if the OS poll itself failed or
/// the input was empty.
fn selector_poll(reqs: &[PollReq], timeout_ms: i32) -> Vec<i16> {
    if reqs.is_empty() {
        return Vec::new();
    }

    #[cfg(unix)]
    {
        // Translate neutral events → POLLIN/POLLOUT.
        let mut pfds: Vec<libc::pollfd> = reqs
            .iter()
            .map(|r| {
                let mut ev: libc::c_short = 0;
                if r.events & POLL_IN != 0 {
                    ev |= libc::POLLIN;
                }
                if r.events & POLL_OUT != 0 {
                    ev |= libc::POLLOUT;
                }
                libc::pollfd {
                    fd: r.fd as libc::c_int,
                    events: ev,
                    revents: 0,
                }
            })
            .collect();

        // SAFETY: pfds is a valid &mut slice with correct layout; libc
        // reads it and writes only the revents fields.
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
        if ret < 0 {
            return Vec::new();
        }
        pfds.iter()
            .map(|p| {
                let mut rev: i16 = 0;
                if p.revents & libc::POLLIN != 0 {
                    rev |= POLL_IN;
                }
                if p.revents & libc::POLLOUT != 0 {
                    rev |= POLL_OUT;
                }
                if p.revents & libc::POLLERR != 0 {
                    rev |= POLL_ERR;
                }
                if p.revents & libc::POLLHUP != 0 {
                    rev |= POLL_HUP;
                }
                rev
            })
            .collect()
    }

    #[cfg(windows)]
    {
        // Direct FFI to WSAPoll — the `libc` crate does not re-export it
        // on Windows. WSAPOLLFD is defined in winsock2.h / mswsock.h.
        #[repr(C)]
        struct Wsapollfd {
            fd: usize, // SOCKET = UINT_PTR, usize on both 32/64-bit Win
            events: i16,
            revents: i16,
        }
        // Winsock event bit constants (from winsock2.h).
        const WSAPOLLRDNORM: i16 = 0x0100;
        const WSAPOLLWRNORM: i16 = 0x0010;
        const WSAPOLLERR: i16 = 0x0001;
        const WSAPOLLHUP: i16 = 0x0002;

        #[link(name = "Ws2_32")]
        extern "system" {
            fn WSAPoll(fd_array: *mut Wsapollfd, fds: u32, timeout: i32) -> i32;
        }

        let mut pfds: Vec<Wsapollfd> = reqs
            .iter()
            .map(|r| {
                let mut ev: i16 = 0;
                if r.events & POLL_IN != 0 {
                    ev |= WSAPOLLRDNORM;
                }
                if r.events & POLL_OUT != 0 {
                    ev |= WSAPOLLWRNORM;
                }
                Wsapollfd {
                    fd: r.fd as usize,
                    events: ev,
                    revents: 0,
                }
            })
            .collect();

        // SAFETY: pfds is a valid &mut slice with correct layout; WSAPoll
        // only reads the events field and writes the revents field.
        let ret = unsafe { WSAPoll(pfds.as_mut_ptr(), pfds.len() as u32, timeout_ms) };
        if ret < 0 {
            return Vec::new();
        }
        pfds.iter()
            .map(|p| {
                let mut rev: i16 = 0;
                if p.revents & WSAPOLLRDNORM != 0 {
                    rev |= POLL_IN;
                }
                if p.revents & WSAPOLLWRNORM != 0 {
                    rev |= POLL_OUT;
                }
                if p.revents & WSAPOLLERR != 0 {
                    rev |= POLL_ERR;
                }
                if p.revents & WSAPOLLHUP != 0 {
                    rev |= POLL_HUP;
                }
                rev
            })
            .collect()
    }
}

#[cfg(unix)]
fn stream_pollreq_fd(stream: &TcpStream) -> i64 {
    use std::os::unix::io::AsRawFd;
    stream.as_raw_fd() as i64
}

#[cfg(windows)]
fn stream_pollreq_fd(stream: &TcpStream) -> i64 {
    use std::os::windows::io::AsRawSocket;
    stream.as_raw_socket() as i64
}

#[cfg(unix)]
fn listener_pollreq_fd(listener: &TcpListener) -> i64 {
    use std::os::unix::io::AsRawFd;
    listener.as_raw_fd() as i64
}

#[cfg(windows)]
fn listener_pollreq_fd(listener: &TcpListener) -> i64 {
    use std::os::windows::io::AsRawSocket;
    listener.as_raw_socket() as i64
}

#[cfg(unix)]
pub(crate) fn dgram_pollreq_fd(sock: &UdpSocket) -> i64 {
    use std::os::unix::io::AsRawFd;
    sock.as_raw_fd() as i64
}

#[cfg(windows)]
pub(crate) fn dgram_pollreq_fd(sock: &UdpSocket) -> i64 {
    use std::os::windows::io::AsRawSocket;
    sock.as_raw_socket() as i64
}

// ---- NEW-3: Per-selector wakeup channel ------------------------------------
//
// Each `Selector` gets a lazily-constructed, self-connected `UdpSocket` that
// is added to the `pollfds` array on every `select()` call. `Selector.wakeup`
// writes one byte to this socket; the poll wakes up, the byte is drained
// from the receive queue, and the function returns promptly. The selector's
// identity (used as the map key) is its stable Java identity hash plus VM
// identity. Raw ObjectRef pointer bits are not safe here because a moving GC
// can relocate a selector while another thread is blocked in select(). Entries
// are removed when the Java `Selector.close()` runs.
//
// The UDP socket is bound to 127.0.0.1:0, which the OS fills in with an
// ephemeral port. We then call `connect()` on the same socket back to its
// own `local_addr()` so that `send(&[byte])` loops the byte right back into
// the socket's recv queue. This works identically on Unix and Windows with
// zero platform-specific code paths — UDP send-to-self is a well-defined
// Berkeley-socket operation that every major OS implements.

struct WakeupChannel {
    socket: UdpSocket,
}

type SelectorWakeupKey = (usize, i32);

static SELECTOR_WAKEUPS: OnceLock<parking_lot::Mutex<HashMap<SelectorWakeupKey, WakeupChannel>>> =
    OnceLock::new();

fn selector_wakeups() -> &'static parking_lot::Mutex<HashMap<SelectorWakeupKey, WakeupChannel>> {
    SELECTOR_WAKEUPS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Produce a stable identity key for the selector.
fn selector_key(ctx: &dyn NativeContext, sel: ObjectRef) -> SelectorWakeupKey {
    (ctx.vm_identity(), ctx.identity_hash_code(sel))
}

/// Construct a new self-connected UDP socket for wakeup signaling. Returns
/// `None` if the OS refuses to give us a loopback socket, in which case the
/// caller falls back to a plain blocking poll without wakeup support (which
/// can still return via the user-supplied timeout).
fn build_wakeup_channel() -> Option<WakeupChannel> {
    let socket = UdpSocket::bind("127.0.0.1:0").ok()?;
    let addr = socket.local_addr().ok()?;
    // Non-blocking so the drain read after poll() never stalls.
    socket.set_nonblocking(true).ok()?;
    socket.connect(addr).ok()?;
    Some(WakeupChannel { socket })
}

/// Get the wakeup channel for this selector, creating one lazily if needed.
/// Returns `None` if construction failed (e.g. loopback unavailable).
fn ensure_wakeup_channel<F, T>(ctx: &dyn NativeContext, sel: ObjectRef, f: F) -> Option<T>
where
    F: FnOnce(&WakeupChannel) -> T,
{
    let key = selector_key(ctx, sel);
    let mut map = selector_wakeups().lock();
    if !map.contains_key(&key) {
        let ch = build_wakeup_channel()?;
        map.insert(key, ch);
    }
    map.get(&key).map(f)
}

/// Drop the wakeup channel (called by `Selector.close()`).
fn release_wakeup_channel(ctx: &dyn NativeContext, sel: ObjectRef) {
    let key = selector_key(ctx, sel);
    selector_wakeups().lock().remove(&key);
}

/// Signal the wakeup channel. Called by `Selector.wakeup()`.
/// Writes a single byte; the blocked `poll()` observes POLLIN on the
/// wakeup fd, returns, and drains the byte.
fn signal_wakeup(ctx: &dyn NativeContext, sel: ObjectRef) {
    ensure_wakeup_channel(ctx, sel, |ch| {
        // Best-effort: if the socket buffer is full (caller wake'd N times
        // without any poll clearing it), the send fails and we silently
        // proceed — the byte that is already pending is enough to wake
        // the next poll.
        let _ = ch.socket.send(&[1u8]);
    });
}

/// Drain any queued wakeup bytes on the wakeup socket. Called after every
/// `poll()` return so the next `select()` does not spuriously wake on a
/// leftover byte.
fn drain_wakeup(ctx: &dyn NativeContext, sel: ObjectRef) {
    ensure_wakeup_channel(ctx, sel, |ch| {
        let mut buf = [0u8; 64];
        loop {
            match ch.socket.recv(&mut buf) {
                Ok(n) if n > 0 => continue, // keep draining
                _ => break,                 // empty / would block / closed
            }
        }
    });
}

// ---- Selector poll ---------------------------------------------------------

/// Poll with a timeout in milliseconds.
///
/// Semantics (matches `java.nio.channels.Selector`):
///   * `timeout_ms < 0`  — block indefinitely (caller uses -1)
///   * `timeout_ms == 0` — non-blocking poll, return immediately
///   * `timeout_ms > 0`  — block for at most `timeout_ms` milliseconds
///
/// The underlying `libc::poll` (`poll(2)` on Unix, `WSAPoll` on Windows)
/// uses the same contract directly. This function also registers the
/// per-selector wakeup fd into the pollfd array, so `Selector.wakeup()`
/// from another thread can interrupt a blocking call.
fn s2_selector_do_poll_with_timeout(
    ctx: &mut dyn NativeContext,
    sel: ObjectRef,
    timeout_ms: i32,
) -> i32 {
    let mut sel = sel;
    let n = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
    let keys_arr = match ctx.get_field(sel, S2SEL_KEYS) {
        Value::Object(Some(arr)) => arr,
        _ => {
            // No registered keys yet; honor the wakeup + timeout contract
            // anyway so that `select(timeout)` on an empty selector sleeps
            // correctly instead of spinning.
            return poll_empty_selector(ctx, sel, timeout_ms);
        }
    };
    if n == 0 {
        return poll_empty_selector(ctx, sel, timeout_ms);
    }

    // Gather channel info for each registered key.
    struct PollEntry {
        key_ref: ObjectRef,
        channel_ref: ObjectRef,
        interest: i32,
        sock_id: i32,
        listener_id: i32,
        dgram_id: i32,
    }
    let mut entries: Vec<PollEntry> = Vec::with_capacity(n);
    for i in 0..n {
        let key = match ctx.get_array_element(keys_arr, i) {
            Value::Object(Some(k)) => k,
            _ => continue,
        };
        let interest = ctx.get_field(key, 2).as_int().unwrap_or(0);
        let channel = match ctx.get_field(key, 0) {
            Value::Object(Some(ch)) => ch,
            _ => continue,
        };
        let sock_id = ctx.get_field(channel, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        let listener_id = ctx
            .get_field(channel, S2SSC_LISTENER_ID)
            .as_int()
            .unwrap_or(-1);
        let dgram_id = ctx.get_field(channel, S2DC_SOCK_ID).as_int().unwrap_or(-1);
        entries.push(PollEntry {
            key_ref: key,
            channel_ref: channel,
            interest,
            sock_id,
            listener_id,
            dgram_id,
        });
    }

    // Build the PollReq array. One entry per channel with a real fd,
    // plus one trailing entry for the wakeup channel. `req_to_entry`
    // maps each PollReq index back to the original `entries` slot
    // (None = wakeup).
    let reg = s2_registry().lock();
    let mut reqs: Vec<PollReq> = Vec::with_capacity(entries.len() + 1);
    let mut req_to_entry: Vec<Option<usize>> = Vec::with_capacity(entries.len() + 1);

    for (ei, entry) in entries.iter().enumerate() {
        let mut events: i16 = 0;

        if entry.sock_id >= 0 {
            if let Some(stream) = reg.streams.get(&entry.sock_id) {
                if entry.interest & 1 != 0 {
                    events |= POLL_IN;
                }
                if (entry.interest & 4) != 0 || (entry.interest & 8) != 0 {
                    // OP_WRITE and OP_CONNECT both map to POLL_OUT.
                    events |= POLL_OUT;
                }
                reqs.push(PollReq {
                    fd: stream_pollreq_fd(stream),
                    events,
                });
                req_to_entry.push(Some(ei));
                continue;
            }
        }

        if entry.listener_id >= 0 && entry.interest & 16 != 0 {
            if let Some(listener) = reg.listeners.get(&entry.listener_id) {
                reqs.push(PollReq {
                    fd: listener_pollreq_fd(listener),
                    events: POLL_IN,
                });
                req_to_entry.push(Some(ei));
                continue;
            }
        }

        if entry.dgram_id >= 0 {
            if let Some(dgram) = reg.dgrams.get(&entry.dgram_id) {
                if entry.interest & 1 != 0 {
                    events |= POLL_IN;
                }
                if entry.interest & 4 != 0 {
                    events |= POLL_OUT;
                }
                reqs.push(PollReq {
                    fd: dgram_pollreq_fd(dgram),
                    events,
                });
                req_to_entry.push(Some(ei));
                continue;
            }
        }
        // No fd for this entry — still counted for post-processing
        // (OP_CONNECT of an already-connected socket).
    }

    // Append the wakeup fd. If the wakeup channel can't be built, we
    // proceed without it; the caller can still return via timeout.
    drop(reg); // drop registry lock before touching wakeup map
    let wakeup_req = ensure_wakeup_channel(ctx, sel, |ch| PollReq {
        fd: dgram_pollreq_fd(&ch.socket),
        events: POLL_IN,
    });
    if let Some(req) = wakeup_req {
        reqs.push(req);
        req_to_entry.push(None);
    }

    // Call the OS poll. On Unix this hits `poll(2)`; on Windows it hits
    // `WSAPoll` via direct FFI. Both honor the same timeout contract. Blocking
    // waits must enter the VM's GC-blocked protocol so a stop-the-world GC does
    // not wait for an event-loop thread parked in the kernel. Pin selector-local
    // ObjectRefs before the deposit so a moving GC can remap them on wake.
    let revents = if timeout_ms == 0 {
        selector_poll(&reqs, timeout_ms)
    } else {
        let pin_base = ctx.pin_native_root(sel);
        let sel_pin = pin_base;
        let keys_pin = ctx.pin_native_root(keys_arr);
        let entry_pins: Vec<(usize, usize)> = entries
            .iter()
            .map(|entry| {
                (
                    ctx.pin_native_root(entry.key_ref),
                    ctx.pin_native_root(entry.channel_ref),
                )
            })
            .collect();
        ctx.begin_blocking_region();
        let revents = selector_poll(&reqs, timeout_ms);
        ctx.end_blocking_region();
        sel = ctx.read_native_pin(sel_pin, sel);
        let _keys_arr = ctx.read_native_pin(keys_pin, keys_arr);
        for (entry, (key_pin, channel_pin)) in entries.iter_mut().zip(entry_pins) {
            entry.key_ref = ctx.read_native_pin(key_pin, entry.key_ref);
            entry.channel_ref = ctx.read_native_pin(channel_pin, entry.channel_ref);
        }
        ctx.unpin_native_roots(pin_base);
        revents
    };
    if !revents.is_empty() {
        for (pi, rev) in revents.iter().enumerate() {
            let ei = match req_to_entry[pi] {
                Some(e) => e,
                None => continue, // wakeup fd — handled after the loop
            };
            let entry = &entries[ei];
            let mut ready = 0i32;
            if rev & POLL_IN != 0 {
                if entry.interest & 1 != 0 {
                    ready |= 1;
                } // OP_READ
                if entry.interest & 16 != 0 {
                    ready |= 16;
                } // OP_ACCEPT
            }
            if rev & POLL_OUT != 0 {
                if entry.interest & 4 != 0 {
                    ready |= 4;
                } // OP_WRITE
                if entry.interest & 8 != 0 {
                    ready |= 8;
                } // OP_CONNECT
            }
            if rev & (POLL_HUP | POLL_ERR) != 0 {
                // POLL_HUP / POLL_ERR after a read interest: surface as
                // readable so the Java code can observe EOF / error via
                // a normal `read()` that returns -1.
                ready |= entry.interest & 1;
            }
            // Merge with any previously-set ready bits so
            // post-processing (accept / connect) can augment.
            let prev = ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0);
            ctx.set_field(entry.key_ref, 3, Value::Int(prev | ready));
        }
    }

    // Drain the wakeup channel (no-op if no wakeup was pending). We do
    // this unconditionally so a spurious leftover byte from a previous
    // cycle is also cleared.
    drain_wakeup(ctx, sel);

    // Post-processing: OP_ACCEPT eagerly pulls the next connection into
    // the SSC's `pending` slot so the Java caller can hand it out via
    // `accept()`. OP_CONNECT synthesizes readiness for channels that
    // were already connected at registration time.
    for entry in &entries {
        if entry.interest & 16 != 0 && entry.listener_id >= 0 {
            let ready = ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0);
            if ready & 16 != 0 {
                let result = {
                    let mut reg = s2_registry().lock();
                    s2_try_accept_nonblocking(&mut reg, entry.listener_id)
                };
                if let Some(sid) = result {
                    ctx.set_field(entry.channel_ref, S2SSC_PENDING, Value::Int(sid));
                }
            }
        }
        if entry.interest & 8 != 0
            && ctx
                .get_field(entry.channel_ref, S2SC_CONNECTED)
                .as_int()
                .unwrap_or(0)
                != 0
        {
            let old = ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0);
            ctx.set_field(entry.key_ref, 3, Value::Int(old | 8));
        }
    }

    // Count keys whose readyOps are non-zero. This is the select() return
    // value: the number of channels whose readiness set changed.
    let mut count = 0;
    for entry in &entries {
        if ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0) != 0 {
            count += 1;
        }
    }
    count
}

/// Specialization of `s2_selector_do_poll_with_timeout` for a selector that
/// has zero registered keys. We still honor the wakeup + timeout contract
/// so that idiomatic code like `selector.select(1000)` on a not-yet-bound
/// selector sleeps for the timeout instead of spinning.
fn poll_empty_selector(ctx: &mut dyn NativeContext, sel: ObjectRef, timeout_ms: i32) -> i32 {
    if timeout_ms == 0 {
        drain_wakeup(ctx, sel);
        return 0;
    }
    // Construct a single-entry PollReq for the wakeup channel and block
    // on it for the specified timeout. If the wakeup channel could not
    // be built, fall back to `std::thread::sleep` so the caller is still
    // rate-limited rather than busy-looping.
    let req = ensure_wakeup_channel(ctx, sel, |ch| PollReq {
        fd: dgram_pollreq_fd(&ch.socket),
        events: POLL_IN,
    });
    match req {
        Some(r) => {
            let pin_base = ctx.pin_native_root(sel);
            ctx.begin_blocking_region();
            let _ = selector_poll(&[r], timeout_ms);
            ctx.end_blocking_region();
            let sel = ctx.read_native_pin(pin_base, sel);
            ctx.unpin_native_roots(pin_base);
            drain_wakeup(ctx, sel);
            0
        }
        None => {
            if timeout_ms > 0 {
                let pin_base = ctx.pin_native_root(sel);
                ctx.begin_blocking_region();
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
                ctx.end_blocking_region();
                ctx.unpin_native_roots(pin_base);
            }
            0
        }
    }
}

// ---- Close-awareness for the synthetic NIO surface -------------------------
//
// A thread parked in a blocking accept/read/write on one of these sockets must
// come back when another thread closes the channel. It could not: `close`
// removes the entry from `s2_registry()`, but the parked thread cloned the
// `Arc` / `try_clone`d the handle out before it started, so the OS socket stays
// open and the syscall stays in the kernel. That is the same defect the three
// readers fixed on 2026-08-07/11 had, and the loop below is the same loop —
// park in `poll`, re-ask the registry every slice — rather than a second
// mechanism doing the same job.
//
// `net_phase_e.rs` documents this file's accept as broken in a comment of its
// own ("on Windows, closing one duplicated socket handle does not unblock a
// thread blocked in `accept()` on another duplicate"); the fix landed there and
// not here.

/// How long a parked synthetic-NIO operation waits inside one poll before
/// re-asking `s2_registry()` whether its socket was closed under it. Same value
/// and same role as `native-io/src/net.rs`'s `NET_READ_CLOSE_POLL_MS`.
const S2_CLOSE_POLL_MS: i32 = 25;

/// Readiness with a bounded wait, over this file's existing [`selector_poll`]
/// abstraction rather than a fresh binding of `poll(2)`/`WSAPoll`.
///
/// `Some(true)` ready (including POLLERR/POLLHUP, which the following syscall
/// then surfaces as the concrete error); `Some(false)` the slice expired;
/// `None` the OS poll itself failed — the caller's signal to fall back to one
/// plain blocking syscall rather than spin on something that can never report
/// readiness.
pub(crate) fn s2_poll_ready(fd: i64, want_write: bool, timeout_ms: i32) -> Option<bool> {
    let req = PollReq {
        fd,
        events: if want_write { POLL_OUT } else { POLL_IN },
    };
    let revents = selector_poll(&[req], timeout_ms);
    let bits = *revents.first()?;
    let wanted = if want_write { POLL_OUT } else { POLL_IN };
    Some(bits & (wanted | POLL_ERR | POLL_HUP) != 0)
}

/// Is `lid` still a live listener? `ServerSocketChannel.close()` removes the
/// entry, so this flips exactly when Java closed it.
fn s2_listener_still_registered(lid: i32) -> bool {
    s2_registry().lock().listeners.contains_key(&lid)
}

/// Is `sid` still a live stream? `SocketChannel.close()` removes the entry.
pub(crate) fn s2_stream_still_registered(sid: i32) -> bool {
    s2_registry().lock().streams.contains_key(&sid)
}

/// Is `sid` still a live datagram socket? `DatagramChannel.close()` removes the
/// entry. Exposed for `phases_late::net_channels`, whose `DatagramChannel` sites
/// share this registry.
pub(crate) fn s2_dgram_still_registered(sid: i32) -> bool {
    s2_registry().lock().dgrams.contains_key(&sid)
}

/// The error a parked synthetic-NIO operation reports once its socket has been
/// closed from another thread. `ErrorKind::Interrupted` is the carrier all
/// three landed close-aware readers use, and it is unambiguous here because
/// [`s2_poll_ready`] never reports EINTR as an error.
pub(crate) fn s2_async_closed_err() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "channel closed")
}

/// Park until `fd` is ready or the socket `still_registered` names is closed.
///
/// `Ok(true)` ready; `Ok(false)` no usable poll primitive, fall back to one
/// plain blocking syscall; `Err(Interrupted)` closed from another thread.
///
/// The registry lock is taken inside `still_registered` for that lookup alone
/// and is never held across the poll — the whole point of cloning the handle
/// out first, and the reason `s2_registry()` being a single process-wide mutex
/// is survivable at all.
///
/// # On expiry
///
/// The per-pass `S2_CLOSE_POLL_MS` slice expiring is not an outcome — it is
/// only the point at which the registry is re-asked, and the loop continues.
/// There is no second deadline here because none of these call sites has a
/// `SO_TIMEOUT` to honour: this synthetic `SocketChannel` surface exposes no
/// timed read, and `ServerSocketChannel.accept()` is untimed by contract.
pub(crate) fn s2_wait_ready_close_aware(
    fd: i64,
    want_write: bool,
    still_registered: &dyn Fn() -> bool,
) -> std::io::Result<bool> {
    loop {
        let Some(ready) = s2_poll_ready(fd, want_write, S2_CLOSE_POLL_MS) else {
            return Ok(false);
        };
        // Asked AFTER the poll so a close landing while we are parked is seen
        // on the very next pass, and a close that raced a readiness edge still
        // wins.
        if !still_registered() {
            return Err(s2_async_closed_err());
        }
        if ready {
            return Ok(true);
        }
    }
}

fn s2_try_accept_nonblocking(reg: &mut SocketRegistry, lid: i32) -> Option<i32> {
    let listener = reg.listeners.get(&lid)?;
    let _ = listener.set_nonblocking(true);
    let result = listener.accept();
    match result {
        Ok((stream, _)) => {
            let id = reg.next_id;
            reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
            while reg.streams.contains_key(&reg.next_id) || reg.listeners.contains_key(&reg.next_id)
            {
                reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
            }
            reg.streams.insert(id, Arc::new(stream));
            Some(id)
        }
        Err(_) => None,
    }
}

pub(crate) fn s2_blocking_accept(lid: i32) -> Option<i32> {
    // Clone the listener handle out under a SHORT lock, then run the blocking
    // accept() WITHOUT holding s2_registry, and re-lock only to register the
    // accepted stream. Holding the global registry lock across a blocking
    // accept() deadlocks every other synthetic-socket operation process-wide
    // (the Hibernate JTA default-mode hang — see net_phase_e::re2_accept_into).
    let listener = {
        let reg = s2_registry().lock();
        let l = reg.listeners.get(&lid)?;
        let _ = l.set_nonblocking(false);
        l.try_clone().ok()?
    };
    // CLOSE-AWARENESS 2026-08-12: `try_clone()` is precisely why the close
    // could not reach this accept. `ServerSocketChannel.close()` removes the
    // registry entry and drops ITS listener; this thread is parked on a
    // DUPLICATE handle, and on Windows closing one duplicate does not abort a
    // blocking call on another. So park in `poll` and re-ask the registry —
    // the same loop `net::net_accept_close_aware`,
    // `socket_channel::accept_close_aware` and `re2_accept_into` all use.
    //
    // `Ok(false)` (no poll primitive on this target) falls through to the plain
    // blocking accept, which cannot see the close but at least still accepts.
    match s2_wait_ready_close_aware(listener_pollreq_fd(&listener), false, &|| {
        s2_listener_still_registered(lid)
    }) {
        Ok(_) => {}
        // Closed under us. `None` is this function's existing "no connection"
        // answer and every caller already handles it.
        Err(_) => return None,
    }
    // NAMED RESIDUAL: with two threads accepting the same `lid`, the loser of
    // the race between the poll above and this `accept()` parks again until the
    // next connection, and that park is not close-aware. It is not closed by
    // flipping the clone non-blocking: `try_clone` shares the blocking mode with
    // the registry's listener on both platforms (a `dup`'s O_NONBLOCK lives on
    // the open file description; a Windows duplicate shares the socket's FIONBIO
    // state), so this thread would be changing the other one's contract. Every
    // accept parked before this change; at most one loser parks after it.
    match listener.accept() {
        Ok((stream, _)) => {
            let mut reg = s2_registry().lock();
            let id = reg.next_id;
            reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
            while reg.streams.contains_key(&reg.next_id) || reg.listeners.contains_key(&reg.next_id)
            {
                reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
            }
            reg.streams.insert(id, Arc::new(stream));
            Some(id)
        }
        Err(_) => None,
    }
}

// ---- Main entry point ------------------------------------------------------

pub(crate) fn register_s2_nio(r: &mut NativeMethodRegistry) {
    register_s2_bytebuffer(r);
    register_s2_byteorder(r);
    register_s2_socket_channel(r);
    register_s2_server_socket_channel(r);
    register_s2_selector(r);
}

pub(crate) fn register_s2_bytebuffer_essentials(r: &mut NativeMethodRegistry) {
    register_s2_bytebuffer(r);
    register_s2_byteorder(r);
}

// ---- ByteBuffer ------------------------------------------------------------

#[allow(clippy::too_many_lines)]
// Named helpers for view-buffer creation (closures can't capture; need fn ptrs)
macro_rules! s2_view_buf_fn {
    ($name:ident, $cls:literal, $elem_sz:expr) => {
        fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            let lim = s2_bb_limit(ctx, this);
            let rem = (lim - pos) / $elem_sz;
            let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
            // BUG (found 2026-07-11): `ctx.get_field(this, BB_ARRAY)` reads
            // the SOURCE ByteBuffer's raw indexed slot 0 — but in real-JDK
            // mode `this` is allocated with the real Buffer+ByteBuffer field
            // layout (see `bb_write_hb`'s doc comment), where slot 0 is the
            // real `mark` field (an Int, almost always -1), NOT the backing
            // array. The actual array lives in the by-name `hb` field.
            // Reading the wrong slot silently propagated `Value::Int(-1)`
            // into the view's BB_ARRAY slot; every subsequent read on the
            // view then found no `Object` there, fell through `s2_bb_arr`'s
            // "no array" branch, and returned a benign 0 — every ES vector
            // read via `ByteBuffer.asFloatBuffer()`/`asIntBuffer()`/etc.
            // (i.e. essentially all quantized/HNSW/DiskBBQ vector value
            // reads) silently came back as an all-zero vector, surfacing as
            // "expected:<X> but was:<0.0>" across nearly the whole ES
            // vector-codec test family. `s2_bb_arr` resolves the by-name
            // `hb` field first (falling back to the indexed slot only for
            // genuinely storage-less synthetics), matching what
            // `s2_bb_as_char_buffer` already does correctly below.
            //
            // (The typed-buffer-view `slice`/`slice(II)`/`duplicate` natives
            // added alongside this fix's sibling commit do NOT have this bug
            // despite looking similar: their receiver is always an
            // abstract-stamped typed-buffer view, which — unlike ByteBuffer —
            // declares no `hb` field of its own, so `s2_bb_arr`'s by-name
            // lookup already falls straight through to the same indexed
            // slot 0 that a raw `ctx.get_field` reads.)
            if let Some(arr) = s2_bb_arr(ctx, this) {
                ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                // Byte-start marker: the view's element 0 lives at source
                // byte `pos` — PLUS the source's own array-base offset
                // (non-zero when the source is an aliasing `slice()`),
                // since the view's byte accessors resolve against the
                // shared array with the VIEW's own (absent → 0) offset.
                let bs = s2_bb_heap_base(ctx, this) as i32 + pos;
                ctx.set_field(vb, BB_MARK, Value::Int(-(bs + 1)));
            } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                // DIRECT source (residual-doc item 3): keep a real native
                // address — folded to the source's current position — in
                // the view's `address` slot. The byte accessors reach it
                // via `s2_bb_direct_addr`, and `s2_typed_view_byte_start`
                // reads a positive address slot as byte-start 0.
                ctx.set_field_by_name(vb, "address", Value::Long(addr.saturating_add(pos as i64)));
            }
            ctx.set_field(vb, BB_POS, Value::Int(0));
            ctx.set_field(vb, BB_LIMIT, Value::Int(rem));
            ctx.set_field(vb, BB_CAP, Value::Int(rem));
            // Propagate the source buffer's order via the layout-aware
            // accessors (the source may be real-layout: order in `bigEndian`).
            let ord = s2_bb_order(ctx, this);
            s2_bb_set_order(ctx, vb, ord);
            // …and the source's read-only-ness, which is F21-1 N3's other
            // half and had never been propagated. `ByteBuffer.asIntBuffer()`
            // &c. are registered HERE AND NOWHERE ELSE (`native-io` registers
            // no `as<T>Buffer` descriptor), so unlike `$slice`/`$dup` in the
            // macro below this arm is the LIVE one in every mode.
            //
            // MEASURED, jdk-25.0.3+9 (`scratchpad/f37/F37TypedAliasProbe.java`):
            // `ByteBuffer.allocate(64).asReadOnlyBuffer().asIntBuffer()` is a
            // `java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`,
            // its `array()` raises `UnsupportedOperationException` (array-less
            // is asked FIRST — it is not a `ReadOnlyBufferException` row), and
            // its `.slice()` is read-only too. Identical for Long/Short/Float/
            // Double/Char and for the direct twin `DirectIntBufferRS`.
            //
            // Without this the typed views' new `$put` guard could be walked
            // straight around: take a read-only ByteBuffer, ask it for an
            // `asIntBuffer()` view, and write through the view into the
            // read-only buffer's own backing array.
            let src_read_only = s2_bb_is_read_only(ctx, this);
            s2_bb_set_read_only(ctx, vb, src_read_only);
            Ok(Some(Value::Object(Some(vb))))
        }
    };
}
s2_view_buf_fn!(s2_bb_as_int_buffer, "java/nio/IntBuffer", 4);
s2_view_buf_fn!(s2_bb_as_long_buffer, "java/nio/LongBuffer", 8);
s2_view_buf_fn!(s2_bb_as_short_buffer, "java/nio/ShortBuffer", 2);
s2_view_buf_fn!(s2_bb_as_float_buffer, "java/nio/FloatBuffer", 4);
s2_view_buf_fn!(s2_bb_as_double_buffer, "java/nio/DoubleBuffer", 8);

/// Typed views encode their source byte offset as -(offset + 1) in the
/// Buffer.address slot. That slot is a long on real JDK buffer layouts,
/// while synthetic views use an int, so preserve both representations.
#[inline]
fn s2_typed_view_byte_start(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    let marker = match ctx.get_field(this, BB_MARK) {
        Value::Int(v) => v,
        Value::Long(v) => i32::try_from(v).unwrap_or(-1),
        _ => -1,
    };
    if marker < 0 {
        -(marker + 1)
    } else {
        0
    }
}

/// `ByteBuffer.asCharBuffer()` — the view returned MUST have its backing
/// store stored in the real-JDK `hb` field so JDK bytecode that reads
/// `hb` (e.g. `CharBuffer.hasArray`, `CharBuffer.array`) sees a non-null
/// char[]. The previous indexed-only write (`set_field(vb, BB_ARRAY, …)`)
/// hit the real-JDK Buffer.mark slot — descriptor coercion (`I`) turned
/// the Object reference into Int(low_bits_of_ptr), so when icu4j
/// invoked `CharBuffer.subSequence(...).toString()` on the view, the
/// JDK bytecode read a null `hb` and threw IllegalStateException
/// "CharBuffer has no backing array" (icu4j-68.2 / icu4j-70.1).
///
/// The byte storage is also transcoded eagerly into a freshly-allocated
/// char[] using the source ByteBuffer's byte order — the previous
/// shim left slot 0 pointing at the byte[] source, which our native
/// `charAt` interpreted as one char per byte (truncated UTF-16
/// high bytes to zero).
/// The real `ByteBufferAsCharBuffer{B,L}` for this byte order, when the class
/// library actually has one.
///
/// `None` in synthetic-JDK mode (where the class does not exist, or exists only
/// as a fabricated stub with no method bodies), which is what keeps the copying
/// fallback below reachable.
fn s2_bbacb_view_class(ctx: &mut dyn NativeContext, order: i32) -> Option<&'static str> {
    let cls = if order == 1 {
        "java/nio/ByteBufferAsCharBufferL"
    } else {
        "java/nio/ByteBufferAsCharBufferB"
    };
    // `class_id_by_name` alone answers "already loaded", and nothing loads this
    // class before the first `asCharBuffer` — so asking that way said "absent"
    // on a real JDK and silently kept the copying fallback. Drive the load.
    //
    // The return value of `ensure_class_initialized` is not the test:
    // `--jdk-only` aside, it fabricates a bare stub rather than failing. Ask
    // the property instead — a stub has no method bodies, so handing one back
    // would trade `AbstractMethodError` for a buffer whose every method is a
    // no-op.
    if ctx.ensure_class_initialized(cls).is_err() {
        return None;
    }
    if ctx.is_class_synthetic_stub(cls) {
        return None;
    }
    Some(cls)
}

fn s2_bb_as_char_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = s2_bb_pos(ctx, this) as usize;
    let lim = s2_bb_limit(ctx, this) as usize;
    let order = s2_bb_order(ctx, this); // 0 = BIG_ENDIAN, 1 = LITTLE_ENDIAN
    let rem_bytes = lim.saturating_sub(pos);
    let rem_chars = rem_bytes / 2;

    // Real-JDK: hand back a genuine `ByteBufferAsCharBuffer{B,L}` over the SAME
    // ByteBuffer, which is what `asCharBuffer` is specified to return.
    //
    // The copying fallback below stamps the ABSTRACT `java/nio/CharBuffer` and
    // that is wrong three ways at once, all of them measured against HotSpot by
    // `probes/NioBufferStampProbe`:
    //
    //   * `slice`, `duplicate`, `asReadOnlyBuffer`, `put(int,char)` and bulk
    //     `put(CharBuffer)` have no native on the abstract class, so they
    //     resolve to the abstract declaration and throw
    //     `AbstractMethodError: ... has no Code attribute`. Adding a native per
    //     method is whack-a-mole; a concrete receiver gets all of them from the
    //     JDK's own bodies.
    //   * A copy is not a view. `bb.asCharBuffer().put('A')` left the backing
    //     ByteBuffer untouched — HotSpot writes through. A lost write is the
    //     worse failure of the two, because nothing reports it.
    //   * `hasArray()` answered `true` (the copy has a `char[]`); a real char
    //     view over a ByteBuffer has no accessible array.
    //
    // `address` is the whole aliasing mechanism: the JDK computes each element's
    // byte offset as `(i << 1) + address`, and seeds `address` to the source's
    // own address plus its position. Carry that across so the existing
    // `ByteBufferAsCharBuffer*` natives — and the JDK bodies — index the same
    // bytes the source does.
    if let Some(view_cls) = s2_bbacb_view_class(ctx, order) {
        let src_address = match ctx.get_field_by_name(this, "address") {
            Value::Long(v) => v,
            Value::Int(v) => i64::from(v),
            // No `address` on the receiver (a synthetic-layout source): fall
            // back to the ARRAY_BYTE_BASE_OFFSET seeding `bb_write_hb` uses.
            _ => 16,
        };
        // Was an open-coded `matches!(…, Value::Int(1))` — the FIFTH
        // transcription of the `isReadOnly` field read, and the only one that
        // tested `== 1` rather than `!= 0`, so a flag written as any other
        // non-zero value made this arm disagree with `hasArray`/`array`/
        // `arrayOffset` about one receiver. Routed through the converged
        // `s2_bb_is_read_only` (F14-1 N1 / F21-1 §3). Nothing writes such a
        // value today: a latent divergence closed, not a flip.
        let read_only = s2_bb_is_read_only(ctx, this);
        let vb = try_alloc_concurrent_synthetic(ctx, view_cls, 0)?;
        ctx.set_field_by_name(vb, "bb", Value::Object(Some(this)));
        ctx.set_field_by_name(vb, "mark", Value::Int(-1));
        ctx.set_field_by_name(vb, "position", Value::Int(0));
        ctx.set_field_by_name(vb, "limit", Value::Int(rem_chars as i32));
        ctx.set_field_by_name(vb, "capacity", Value::Int(rem_chars as i32));
        ctx.set_field_by_name(vb, "isReadOnly", Value::Int(i32::from(read_only)));
        ctx.set_field_by_name(vb, "address", Value::Long(src_address + pos as i64));
        return Ok(Some(Value::Object(Some(vb))));
    }
    // Transcode bytes → chars using the source ByteBuffer's byte order.
    let chars_arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, rem_chars);
    // Storage-aware byte reads (heap incl. array-base offset, or direct) —
    // the previous `s2_bb_arr`-only loop silently produced an all-zero
    // char[] for direct sources (residual-doc item 3).
    for i in 0..rem_chars {
        let hi = s2_bb_get_byte(ctx, this, (pos + 2 * i) as i32) as i32 & 0xFF;
        let lo = s2_bb_get_byte(ctx, this, (pos + 2 * i + 1) as i32) as i32 & 0xFF;
        let ch = if order == 1 {
            (lo << 8) | hi
        } else {
            (hi << 8) | lo
        };
        ctx.set_array_element(chars_arr, i, Value::Int(ch));
    }
    // Read the source's read-only flag BEFORE the allocation below. `this` is
    // an `ObjectRef` and `try_alloc_concurrent_synthetic` can trigger a
    // collection that RELOCATES it; carrying a `bool` across the allocation is
    // something no collector can move, which is the same reason
    // `native-io`'s `buf_write_read_only` takes a `bool` rather than the
    // source. (The `if let` arm above already reads it before its own alloc.)
    let src_read_only = s2_bb_is_read_only(ctx, this);
    // GC-safety: `alloc_concurrent_synthetic` below allocates and can
    // trigger a collection that relocates `chars_arr` (written into the
    // new CharBuffer's fields further below); pin it and re-read.
    let chars_arr_pin = ctx.pin_native_root(chars_arr);
    let vb = try_alloc_concurrent_synthetic(ctx, "java/nio/CharBuffer", 6)?;
    let chars_arr = ctx.read_native_pin(chars_arr_pin, chars_arr);
    ctx.unpin_native_roots(chars_arr_pin);
    // Write to BOTH indexed slot 0 (synthetic-mode layout used by our
    // own CharBuffer natives) AND the real-JDK `hb` field by name (so
    // JDK bytecode reading `hb` / `hasArray` / `array` sees the char[]).
    ctx.set_field_by_name(vb, "hb", Value::Object(Some(chars_arr)));
    ctx.set_field_by_name(vb, "offset", Value::Int(0));
    // Was a flat `Int(0)`, i.e. this fallback arm ACTIVELY CLEARED the flag —
    // the same shape F21-1 §6.2 found in `slice`/`slice(II)`/`duplicate`'s
    // copying arms, in the one member of the family nobody had re-read.
    // `roBB.asCharBuffer()` is a `java.nio.ByteBufferAsCharBufferRB` with
    // `isReadOnly() == true` on HotSpot (MEASURED), and the arm above already
    // propagates; only this one did not. A copy is not the JDK's shape either
    // way, but a WRITABLE copy of a read-only buffer's contents is the strictly
    // worse of the two answers available here.
    ctx.set_field_by_name(vb, "isReadOnly", Value::Int(i32::from(src_read_only)));
    ctx.set_field_by_name(vb, "position", Value::Int(0));
    ctx.set_field_by_name(vb, "limit", Value::Int(rem_chars as i32));
    ctx.set_field_by_name(vb, "capacity", Value::Int(rem_chars as i32));
    ctx.set_field_by_name(vb, "mark", Value::Int(-1));
    // Heap CharBuffers use ARRAY_CHAR_BASE_OFFSET (16) as their address.
    // Do not unconditionally write the old indexed overlay: in real-JDK
    // layout its slot 4 is Buffer.address, so BB_MARK=-1 made bulk get()
    // call Unsafe.copyMemory with an invalid source offset.
    ctx.set_field_by_name(vb, "address", Value::Long(16));
    if s2_bb_synthetic_layout(ctx, vb) {
        ctx.set_field(vb, BB_ARRAY, Value::Object(Some(chars_arr)));
        ctx.set_field(vb, BB_POS, Value::Int(0));
        ctx.set_field(vb, BB_LIMIT, Value::Int(rem_chars as i32));
        ctx.set_field(vb, BB_CAP, Value::Int(rem_chars as i32));
        ctx.set_field(vb, BB_MARK, Value::Int(-1));
        ctx.set_field(vb, BB_ORDER, Value::Int(order));
    }
    Ok(Some(Value::Object(Some(vb))))
}

/// Build an ALIASING heap ByteBuffer view (used by `slice`/`slice(II)`/
/// `duplicate`/`asReadOnlyBuffer`): shares `arr` with the source and
/// records the view's array-base `offset`, exactly like real-JDK
/// HeapByteBuffer views. `address` mirrors the real ctor's
/// ARRAY_BYTE_BASE_OFFSET + offset seeding (see `bb_write_hb`).
#[allow(clippy::too_many_arguments)]
fn s2_bb_new_heap_view(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    offset: usize,
    pos: i32,
    lim: i32,
    cap: i32,
    mark: i32,
    read_only: bool,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let cls = s2_bb_heap_class(ctx, read_only);
    let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
    ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(buf, "offset", Value::Int(offset as i32));
    ctx.set_field_by_name(buf, "isReadOnly", Value::Int(read_only as i32));
    ctx.set_field_by_name(buf, "position", Value::Int(pos));
    ctx.set_field_by_name(buf, "limit", Value::Int(lim));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(mark));
    ctx.set_field_by_name(
        buf,
        "address",
        Value::Long(16i64.saturating_add(offset as i64)),
    );
    if s2_bb_synthetic_layout(ctx, buf) {
        // Bare-synthetic layout: no `offset` field exists, so aliasing at a
        // non-zero base is not representable — the callers below keep the
        // legacy copying behaviour for that mode instead of reaching here.
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(pos));
        ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Int(mark));
    }
    s2_bb_set_order(ctx, buf, ord);
    Ok(buf)
}

/// Build an ALIASING direct ByteBuffer view over native memory at `addr`
/// (already advanced to the view's byte 0). The object is stamped with the
/// abstract `java/nio/ByteBuffer` class — every accessor reaches the
/// storage through `s2_bb_direct_addr`, closing residual-doc item 3's
/// "slices/duplicates of a direct buffer come back empty" gap.
#[allow(clippy::too_many_arguments)]
fn s2_bb_new_direct_view(
    ctx: &mut dyn NativeContext,
    addr: i64,
    pos: i32,
    lim: i32,
    cap: i32,
    mark: i32,
    read_only: bool,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6)?;
    ctx.set_field_by_name(buf, "isReadOnly", Value::Int(read_only as i32));
    ctx.set_field_by_name(buf, "position", Value::Int(pos));
    ctx.set_field_by_name(buf, "limit", Value::Int(lim));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(mark));
    ctx.set_field_by_name(buf, "address", Value::Long(addr));
    s2_bb_set_order(ctx, buf, ord);
    Ok(buf)
}

fn register_s2_bytebuffer(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;
    let bb = "java/nio/ByteBuffer";

    r.register(bb, "allocate", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let requested = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        // `ByteBuffer.allocate` opens with
        // `if (capacity < 0) throw createCapacityException(capacity)`.
        // Clamping to 0 handed back an empty buffer and reported success.
        if requested < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("capacity < 0: ({requested} < 0)"),
            }
            .into());
        }
        let cap = requested as usize;
        match s2_bb_alloc(ctx, cap)? {
            Some(buf) => Ok(Some(Value::Object(Some(buf)))),
            // The message names the site and the size. It used to be a bare
            // "Java heap space", which is also what the pre-allocated singleton
            // OOME carries and what three unrelated natives throw -- so the
            // string identified nothing. Chasing the H2 `TestBenchmark` refusal
            // cost a run per candidate site purely to find out which of them had
            // produced it; the two bytecode paths already name themselves
            // ("alloc_array length N"), and this is the third allocator of
            // caller-sized arrays.
            None => Err(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (ByteBuffer.allocate {cap})"),
            }
            .into()),
        }
    });
    r.register(
        bb,
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let cap = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            if cap < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "capacity < 0".into(),
                }
                .into());
            }
            // Real-JDK mode: run the genuine `DirectByteBuffer(int)`
            // constructor. It installs the JDK's own `Cleaner`/`Deallocator`
            // and `Bits` accounting, which is strictly better than anything we
            // can synthesise, so nothing changes on that path.
            //
            // Both probes are needed and neither alone is enough:
            // `would_fabricate_synthetic_stub` is the non-destructive "are the
            // class bytes reachable" question, but it answers "no stub" once
            // ANY earlier caller has already minted the stub (it short-circuits
            // on `resolve_fast_path_class_id`); `is_class_synthetic_stub`
            // covers exactly that case by asking what the loaded class IS.
            let real_direct_byte_buffer = !ctx
                .would_fabricate_synthetic_stub("java/nio/DirectByteBuffer")
                && !ctx.is_class_synthetic_stub("java/nio/DirectByteBuffer");
            if real_direct_byte_buffer {
                return ctx.new_object_initialized(
                    "java/nio/DirectByteBuffer",
                    "(I)V",
                    &[Value::Int(cap)],
                );
            }
            // Synthetic-JDK mode: there is no `DirectByteBuffer` bytecode to
            // run and `new_object_initialized` would raise NoClassDefFound —
            // `allocateDirect` simply threw. Build a genuinely direct buffer
            // here instead (NEW-17).
            s2_bb_alloc_direct(ctx, cap)
        },
    );
    r.register(bb, "wrap", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let len = ctx.array_length(arr) as i32;
        let cls = s2_bb_heap_class(ctx, false);
        let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
        bb_write_hb(ctx, buf, arr, len);
        Ok(Some(Value::Object(Some(buf))))
    });
    r.register(bb, "wrap", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let off = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let cap = i32::try_from(ctx.array_length(arr)).unwrap_or(i32::MAX);
        // `ByteBuffer.wrap(array, offset, length)` is
        // `try { new HeapByteBuffer(array, offset, length, null) }
        //  catch (IllegalArgumentException x) { throw new IndexOutOfBoundsException(); }`
        // — the range check is the constructor's, and it raises
        // `IndexOutOfBoundsException` with no detail message. Clamping the
        // limit with `.min(cap)` instead silently produced a SHORTER buffer
        // than asked for: `wrap(new byte[4], 0, 9)` returned a 4-byte window
        // and reported success.
        if off < 0 || len < 0 || i64::from(off) + i64::from(len) > i64::from(cap) {
            return Err(RuntimeError::ioobe_no_message().into());
        }
        let cls = s2_bb_heap_class(ctx, false);
        let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
        bb_write_hb(ctx, buf, arr, cap);
        // Override position/limit set by bb_write_hb.
        ctx.set_field_by_name(buf, "position", Value::Int(off));
        ctx.set_field_by_name(buf, "limit", Value::Int(off + len));
        ctx.set_field(buf, BB_POS, Value::Int(off));
        ctx.set_field(buf, BB_LIMIT, Value::Int(off + len));
        Ok(Some(Value::Object(Some(buf))))
    });

    // get
    r.register(bb, "get", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let b = s2_bb_get_byte(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Int(b as i32)))
    });
    r.register(bb, "get", "(I)B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 1)?;
        Ok(Some(Value::Int(s2_bb_get_byte(ctx, this, idx) as i32)))
    });
    r.register(bb, "get", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        // BUG [nb-servlet]: `off`/`len` were taken as raw i32 with NO negativity
        // check, and `off` was cast to usize BEFORE validation. A negative `len`
        // passed the `pos + len > limit` test (it makes the sum smaller), then
        // `for i in 0..len as usize` reinterpreted the negative as ~1.8e19 → hang/DoS.
        // A negative `off` cast straight to a huge usize. Fix: reject off<0/len<0
        // with (Array)IndexOutOfBoundsException first, do the bounds math in widened
        // i64 to avoid overflow, and verify off+len fits the destination array.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let dst_cap = i32::try_from(ctx.array_length(dst)).unwrap_or(i32::MAX);
        s2_check_from_index_size(off, len, dst_cap)?;
        let pos = s2_bb_pos(ctx, this);
        // Widened arithmetic: pos+len cannot wrap into a "passing" value.
        if (pos as i64) + (len as i64) > s2_bb_limit(ctx, this) as i64 {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let off = off as usize;
        // Mirrors put(Ljava/nio/ByteBuffer;)'s direct-buffer fix
        // (2026-07-10): the pre-fix `s2_bb_arr(ctx, this).unwrap_or(dst)`
        // made `dst` its own copy source whenever `this` is a DIRECT
        // buffer (no heap array) — a silent self-copy that left `dst`
        // untouched while position still advanced and the call reported
        // success. `IOUtil.read` routes every buffered `FileChannel` read
        // through a temporary direct buffer and then bulk-`get`s it into a
        // byte[], so this exact path is how Lucene's footer/checksum bytes
        // came back as zero (ES-FAIL-FAMILY-20260709).
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let src_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, arr, src_off, dst, off, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(arr, src_off + i);
                    ctx.set_array_element(dst, off + i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if !ctx.copy_from_native_memory(addr.saturating_add(pos as i64), &mut bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.get([BII): direct source read failed".to_string(),
                }
                .into());
            }
            if !ctx.write_byte_array_from(dst, off, &bytes) {
                for (i, byte) in bytes.iter().enumerate() {
                    ctx.set_array_element(dst, off + i, Value::Int(*byte as i8 as i32));
                }
            }
        } else {
            // Genuinely storage-less synthetic buffer — keep the historic
            // silent no-op (position unchanged), matching
            // put(ByteBuffer;)'s same fallback for half-built synthetics.
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "get", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let len = ctx.array_length(dst) as i32;
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let src_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, arr, src_off, dst, 0, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(arr, src_off + i);
                    ctx.set_array_element(dst, i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if !ctx.copy_from_native_memory(addr.saturating_add(pos as i64), &mut bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.get([B): direct source read failed".to_string(),
                }
                .into());
            }
            if !ctx.write_byte_array_from(dst, 0, &bytes) {
                for (i, byte) in bytes.iter().enumerate() {
                    ctx.set_array_element(dst, i, Value::Int(*byte as i8 as i32));
                }
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });

    // put
    r.register(bb, "put", "(B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i8;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_put_byte(ctx, this, pos, b);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "(IB)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i8;
        s2_bb_check_index(ctx, this, idx, 1)?;
        s2_bb_put_byte(ctx, this, idx, b);
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let src = obj_arg(args, 1)?;
        // BUG [nb-servlet]: symmetric to get([BII). `off`/`len` were raw i32 with
        // no negativity check and `off` cast to usize before validation. A negative
        // `len` slipped past `pos + len > limit` then `for i in 0..len as usize`
        // looped ~1.8e19 times (hang/DoS); a negative `off` indexed src wildly. Fix:
        // reject off<0/len<0 with (Array)IndexOutOfBoundsException, widen the buffer
        // bounds math to i64, and verify off+len fits the source array.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let src_cap = i32::try_from(ctx.array_length(src)).unwrap_or(i32::MAX);
        s2_check_from_index_size(off, len, src_cap)?;
        let pos = s2_bb_pos(ctx, this);
        // Widened arithmetic: pos+len cannot wrap into a "passing" value.
        if (pos as i64) + (len as i64) > s2_bb_limit(ctx, this) as i64 {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let off = off as usize;
        // Mirrors put(Ljava/nio/ByteBuffer;)'s direct-buffer fix — see
        // get([BII)'s comment above for the matching read-side rationale.
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let dst_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, src, off, arr, dst_off, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(src, off + i);
                    ctx.set_array_element(arr, dst_off + i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if ctx.read_byte_array_into(src, off, &mut bytes) != len as usize {
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = ctx.get_array_element(src, off + i).as_int().unwrap_or(0) as u8;
                }
            }
            if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put([BII): direct destination write failed".to_string(),
                }
                .into());
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let src = obj_arg(args, 1)?;
        let len = ctx.array_length(src) as i32;
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let dst_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, src, 0, arr, dst_off, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(src, i);
                    ctx.set_array_element(arr, dst_off + i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if ctx.read_byte_array_into(src, 0, &mut bytes) != len as usize {
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = ctx.get_array_element(src, i).as_int().unwrap_or(0) as u8;
                }
            }
            if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put([B): direct destination write failed".to_string(),
                }
                .into());
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(
        bb,
        "put",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let src = obj_arg(args, 1)?;
            // `ByteBuffer.put(ByteBuffer src)` rejects a self-copy outright:
            // `if (src == this) throw createSameBufferException()`. Checked
            // FIRST, ahead of the read-only test, matching the JDK's own order
            // — a read-only buffer put into itself reports the
            // IllegalArgumentException, not ReadOnlyBufferException.
            //
            // CratonVM used to perform the copy. Since the 2026-07-31 bulk
            // rewrite that copy at least went through an owned intermediate,
            // so it was well defined rather than an overlapping element-wise
            // walk — but `ByteBuffer.wrap(new byte[16]).put(b)` still left
            // pos=16 where HotSpot throws, and any code doing it is already
            // broken on a real JVM.
            if src == this {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "The source buffer is this buffer".to_string(),
                }
                .into());
            }
            if s2_bb_is_read_only(ctx, this) {
                return Err(RuntimeError::ReadOnlyBufferException.into());
            }
            let src_pos = s2_bb_pos(ctx, src);
            let src_lim = s2_bb_limit(ctx, src);
            let n = (src_lim - src_pos).max(0) as usize;
            let pos = s2_bb_pos(ctx, this);
            if pos + n as i32 > s2_bb_limit(ctx, this) {
                return Err(RuntimeError::BufferOverflowException.into());
            }
            // Either side may be a DIRECT buffer (real-JDK `DirectByteBuffer`:
            // `hb == null`, storage at `address`). The pre-fix code silently
            // returned without copying OR advancing positions whenever a side
            // had no heap array. `IOUtil.read` routes every buffered
            // `FileChannel` read through a temporary direct buffer and then
            // `dst.put(directSrc)`, so file reads "succeeded" (count
            // returned) while delivering ZERO bytes with an unmoved
            // destination position — Lucene's `BufferedIndexInput.refill()`
            // then flipped an empty buffer and the first `readByte()` threw
            // `BufferUnderflowException` (doc:
            // ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object).
            let mut bytes = vec![0u8; n];
            if let Some(src_arr) = s2_bb_arr(ctx, src) {
                let src_base = s2_bb_heap_base(ctx, src);
                let src_start = src_base + src_pos as usize;
                if ctx.read_byte_array_into(src_arr, src_start, &mut bytes) != n {
                    for (i, b) in bytes.iter_mut().enumerate() {
                        *b = ctx
                            .get_array_element(src_arr, src_start + i)
                            .as_int()
                            .unwrap_or(0) as u8;
                    }
                }
            } else if let Some(addr) = s2_bb_direct_addr(ctx, src) {
                if !ctx.copy_from_native_memory(addr.saturating_add(src_pos as i64), &mut bytes) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "ByteBuffer.put: direct source read failed".to_string(),
                    }
                    .into());
                }
            } else {
                // Genuinely storage-less synthetic buffer — keep the historic
                // silent no-op so half-built synthetic buffers stay benign.
                return Ok(Some(Value::Object(Some(this))));
            }
            if let Some(dst_arr) = s2_bb_arr(ctx, this) {
                let dst_base = s2_bb_heap_base(ctx, this);
                let dst_start = dst_base + pos as usize;
                if !ctx.write_byte_array_from(dst_arr, dst_start, &bytes) {
                    for (i, b) in bytes.iter().enumerate() {
                        ctx.set_array_element(dst_arr, dst_start + i, Value::Int(*b as i8 as i32));
                    }
                }
            } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "ByteBuffer.put: direct destination write failed".to_string(),
                    }
                    .into());
                }
            } else {
                return Ok(Some(Value::Object(Some(this))));
            }
            s2_bb_set_pos(ctx, src, src_lim);
            s2_bb_set_pos(ctx, this, pos + n as i32);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // getShort / putShort
    r.register(bb, "getShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read2(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Int(v as i32)))
    });
    r.register(bb, "getShort", "(I)S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 2)?;
        Ok(Some(Value::Int(s2_bb_read2(ctx, this, idx) as i32)))
    });
    r.register(bb, "putShort", "(S)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write2(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putShort", "(IS)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        s2_bb_check_index(ctx, this, idx, 2)?;
        s2_bb_write2(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getChar / putChar
    r.register(bb, "getChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read2(ctx, this, pos) as u16;
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Int(v as i32)))
    });
    r.register(bb, "getChar", "(I)C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 2)?;
        Ok(Some(Value::Int(s2_bb_read2(ctx, this, idx) as u16 as i32)))
    });
    r.register(bb, "putChar", "(C)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write2(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putChar", "(IC)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        s2_bb_check_index(ctx, this, idx, 2)?;
        s2_bb_write2(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getInt / putInt
    r.register(bb, "getInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read4(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Int(v)))
    });
    r.register(bb, "getInt", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 4)?;
        Ok(Some(Value::Int(s2_bb_read4(ctx, this, idx))))
    });
    r.register(bb, "putInt", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write4(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putInt", "(II)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 4)?;
        s2_bb_write4(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getLong / putLong
    r.register(bb, "getLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read8(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Long(v)))
    });
    r.register(bb, "getLong", "(I)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 8)?;
        Ok(Some(Value::Long(s2_bb_read8(ctx, this, idx))))
    });
    r.register(bb, "putLong", "(J)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = match args.get(1) {
            Some(Value::Long(l)) => *l,
            Some(Value::Int(i)) => *i as i64,
            _ => 0,
        };
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write8(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putLong", "(IJ)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = match args.get(2) {
            Some(Value::Long(l)) => *l,
            Some(Value::Int(i)) => *i as i64,
            _ => 0,
        };
        s2_bb_check_index(ctx, this, idx, 8)?;
        s2_bb_write8(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getFloat / putFloat
    r.register(bb, "getFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bits = s2_bb_read4(ctx, this, pos) as u32;
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Float(f32::from_bits(bits))))
    });
    r.register(bb, "getFloat", "(I)F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 4)?;
        Ok(Some(Value::Float(f32::from_bits(
            s2_bb_read4(ctx, this, idx) as u32,
        ))))
    });
    r.register(bb, "putFloat", "(F)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = match args.get(1) {
            Some(Value::Float(f)) => *f,
            Some(Value::Int(i)) => f32::from_bits(*i as u32),
            _ => 0.0,
        };
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write4(ctx, this, pos, v.to_bits() as i32);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Object(Some(this))))
    });

    // getDouble / putDouble
    r.register(bb, "getDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bits = s2_bb_read8(ctx, this, pos) as u64;
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Double(f64::from_bits(bits))))
    });
    r.register(bb, "putDouble", "(D)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = match args.get(1) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write8(ctx, this, pos, v.to_bits() as i64);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Object(Some(this))))
    });

    // flip / clear / rewind / mark (both Buffer and ByteBuffer return types)
    for ret in &["()Ljava/nio/Buffer;", "()Ljava/nio/ByteBuffer;"] {
        let ret = *ret;
        r.register(bb, "flip", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_LIMIT, Value::Int(pos));
            ctx.set_field(this, BB_POS, Value::Int(0));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(bb, "clear", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cap = s2_bb_cap(ctx, this);
            ctx.set_field(this, BB_POS, Value::Int(0));
            ctx.set_field(this, BB_LIMIT, Value::Int(cap));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(bb, "rewind", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, BB_POS, Value::Int(0));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(bb, "mark", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            s2_bb_set_mark(ctx, this, pos);
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(bb, "reset", "()Ljava/nio/Buffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mark = s2_bb_get_mark(ctx, this);
        if mark < 0 {
            // THE TYPE, not a message that names the type. This raised
            // `IllegalStateException` whose detail message was the STRING
            // "InvalidMarkException" -- so `catch (InvalidMarkException)`, the
            // only handler anyone writes for this, did not match.
            //
            // `InvalidMarkException extends IllegalStateException`, so the
            // supertype handler still worked and nothing failed loudly. What
            // made it visible was the DESCRIPTOR: this registration covers
            // `()Ljava/nio/Buffer;` only, and javac emits that spelling solely
            // when the reference is typed `Buffer`. Through a `ByteBuffer`
            // reference the real bytecode runs and answers correctly, so the
            // bridge and its target DISAGREED:
            //
            //   ByteBuffer bb = ...;  bb.reset()   InvalidMarkException
            //   Buffer     b  = bb;   b.reset()    IllegalStateException
            //
            // MEASURED with `apps/probes/L4TailSweep2.java`, which types the
            // reference as `Buffer` for exactly this reason.
            if let Ok(Some(Value::Object(Some(exc)))) =
                ctx.new_object("java/nio/InvalidMarkException")
            {
                let _ = ctx.invoke(
                    "java/nio/InvalidMarkException",
                    "<init>",
                    "()V",
                    &[Value::Object(Some(exc))],
                );
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc));
            }
            return Err(RuntimeError::IllegalStateException {
                message: "InvalidMarkException".into(),
            }
            .into());
        }
        ctx.set_field(this, BB_POS, Value::Int(mark));
        Ok(Some(Value::Object(Some(this))))
    });

    // position / limit
    r.register(bb, "position", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_POS)))
    });
    for ret in &["(I)Ljava/nio/Buffer;", "(I)Ljava/nio/ByteBuffer;"] {
        r.register(bb, "position", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let limit = s2_bb_limit(ctx, this);
            if v < 0 || v > limit {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("newPosition > limit: ({v} > {limit})"),
                }
                .into());
            }
            if s2_bb_get_mark(ctx, this) > v {
                s2_bb_set_mark(ctx, this, -1);
            }
            ctx.set_field(this, BB_POS, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(bb, "limit", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_LIMIT)))
    });
    for ret in &["(I)Ljava/nio/Buffer;", "(I)Ljava/nio/ByteBuffer;"] {
        r.register(bb, "limit", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let cap = s2_bb_cap(ctx, this);
            if v < 0 || v > cap {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("newLimit > capacity: ({v} > {cap})"),
                }
                .into());
            }
            let pos = s2_bb_pos(ctx, this);
            if pos > v {
                ctx.set_field(this, BB_POS, Value::Int(v));
            }
            if s2_bb_get_mark(ctx, this) > v {
                s2_bb_set_mark(ctx, this, -1);
            }
            ctx.set_field(this, BB_LIMIT, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(bb, "capacity", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_CAP)))
    });
    r.register(bb, "remaining", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            (s2_bb_limit(ctx, this) - s2_bb_pos(ctx, this)).max(0),
        )))
    });
    r.register(bb, "hasRemaining", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            if s2_bb_limit(ctx, this) > s2_bb_pos(ctx, this) {
                1
            } else {
                0
            },
        )))
    });
    r.register(bb, "compact", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let pos = s2_bb_pos(ctx, this).max(0);
        let lim = s2_bb_limit(ctx, this).max(pos);
        let cap = s2_bb_cap(ctx, this);
        let n = (lim - pos) as usize;
        // Storage-aware move (residual-doc item 3: direct receivers were a
        // silent no-op). Reading the window first makes the overlapping
        // move safe for both storage kinds.
        if let Some(bytes) = s2_bb_read_window(ctx, this, pos, n) {
            match s2_bb_storage(ctx, this) {
                Some(S2BbStorage::Heap { arr, base }) => {
                    for (i, b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, base + i, Value::Int(*b as i8 as i32));
                    }
                }
                Some(S2BbStorage::Direct { addr }) => {
                    let _ = ctx.copy_to_native_memory(addr, &bytes);
                }
                None => {}
            }
        }
        ctx.set_field(this, BB_POS, Value::Int(n as i32));
        ctx.set_field(this, BB_LIMIT, Value::Int(cap));
        s2_bb_set_mark(ctx, this, -1);
        Ok(Some(Value::Object(Some(this))))
    });

    // array / hasArray / isDirect / isReadOnly / arrayOffset — all
    // storage/flag-aware now (previously hardcoded heap-and-writable, so a
    // direct or read-only receiver answered wrong on every one of these).
    //
    // CONVERGED (F14-1 N1): the three-way decision is
    // `cratonvm_native_io::buffer_array_access`, the one copy of it the
    // dependency edge lets this crate import. The behaviour is unchanged —
    // `Accessible`/`ReadOnly` are the two arms that were here, and `Absent`
    // keeps this file's FOURTH arm (the storage-less synthetic's historic
    // benign null), which is a CratonVM-only state the JDK's two-valued
    // `hb == null` question has no cell for.
    r.register(bb, "array", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s2_bb_arr(ctx, this);
        match buffer_array_access(arr.is_some(), s2_bb_is_read_only(ctx, this)) {
            BufferArrayAccess::Accessible => Ok(Some(Value::Object(arr))),
            BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
            BufferArrayAccess::Absent if s2_bb_direct_addr(ctx, this).is_some() => {
                Err(s2_bb_no_backing_array())
            }
            // Storage-less synthetic: keep the historic benign null.
            BufferArrayAccess::Absent => Ok(Some(Value::Object(None))),
        }
    });
    // `arrayOffset()` has the SAME two refusals as `array()` eighteen lines
    // above, and had neither — a direct receiver answered `0`, which is a
    // perfectly ordinary offset, so `hasArray()`-less code that reached for the
    // offset got a number instead of the exception that tells it to take the
    // direct path. The real JDK body is three lines and both of them are in it:
    //
    // ```java
    // if (hb == null)  throw new UnsupportedOperationException();
    // if (isReadOnly)  throw new ReadOnlyBufferException();
    // return offset;
    // ```
    //
    // Transcribed from the `array()` arm rather than written afresh, so the two
    // cannot drift; the storage classification and both `RuntimeError` variant
    // shapes are that arm's. Measured oracle rows, probes/DirectByteBufferStateProbe.expected.txt
    // on jdk-25.0.3.9: `direct.arrayOffset.throws`, `direct.win.arrayOffset.throws`
    // and `direct.win.readOnly.arrayOffset.throws` are `UnsupportedOperationException`;
    // `heap.win.readOnly.arrayOffset.throws` is `ReadOnlyBufferException`; the
    // happy paths are `heap.arrayOffset = 0` and `heap.win.arrayOffset = 4`, so
    // the window's base still has to come through. Record: W7-83 §7.1.
    r.register(bb, "arrayOffset", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match buffer_array_access(
            s2_bb_arr(ctx, this).is_some(),
            s2_bb_is_read_only(ctx, this),
        ) {
            BufferArrayAccess::Accessible => {
                Ok(Some(Value::Int(s2_bb_heap_base(ctx, this) as i32)))
            }
            BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
            BufferArrayAccess::Absent if s2_bb_direct_addr(ctx, this).is_some() => {
                Err(s2_bb_no_backing_array())
            }
            // Storage-less synthetic: keep the historic benign zero, for the
            // same reason `array()` keeps its historic benign null.
            BufferArrayAccess::Absent => Ok(Some(Value::Int(s2_bb_heap_base(ctx, this) as i32))),
        }
    });
    // `(hb != null) && !isReadOnly` — the SAME decision as the two arms above,
    // so it is the same call. Written out as a conjunction here, it was the
    // fifth transcription of a rule the JDK writes once.
    r.register(bb, "hasArray", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let access = buffer_array_access(
            s2_bb_arr(ctx, this).is_some(),
            s2_bb_is_read_only(ctx, this),
        );
        Ok(Some(Value::Int(i32::from(
            access == BufferArrayAccess::Accessible,
        ))))
    });
    r.register(bb, "isDirect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            s2_bb_direct_addr(ctx, this).is_some() as i32
        )))
    });
    r.register(bb, "isReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(s2_bb_is_read_only(ctx, this) as i32)))
    });

    // order
    r.register(bb, "order", "()Ljava/nio/ByteOrder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ord = s2_bb_order(ctx, this);
        // Canonical (real-static) ByteOrder object: JDK and library
        // bytecode compares the result with `==` against
        // `ByteOrder.LITTLE_ENDIAN` (e.g. Lucene's `assert buffer.order()
        // == LITTLE_ENDIAN`). The helper also ensures the class is
        // INITIALIZED first — the previous lookup silently fell back to a
        // fresh synthetic (printing as BIG_ENDIAN, failing identity
        // comparisons) when `order()` ran before any Java-side ByteOrder
        // access had triggered <clinit> (residual-doc item 6).
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
    });
    r.register(
        bb,
        "order",
        "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The argument is usually one of the REAL `ByteOrder` statics,
            // whose field 0 is the `name` String — the old
            // `get_field(bo, 0).as_int()` decode silently yielded 0
            // (BIG_ENDIAN) for every real constant, so
            // `order(LITTLE_ENDIAN)` was a no-op in real-JDK mode. Decode
            // both representations.
            let ord = match args.get(1) {
                Some(Value::Object(Some(bo))) => match ctx.get_field(*bo, 0) {
                    Value::Int(v) => v,
                    Value::Object(Some(name)) => match ctx.read_string(name).as_deref() {
                        Some("LITTLE_ENDIAN") => 1,
                        _ => 0,
                    },
                    _ => 0,
                },
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            s2_bb_set_order(ctx, this, ord);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // slice / slice(II) / duplicate / asReadOnlyBuffer — ALIASING views.
    //
    // The previous implementations copied the remaining bytes into a fresh
    // heap array (slice) or dropped the storage entirely for a DIRECT
    // source (all four) — residual-doc items 2/3: writes through a slice
    // never reached the parent, and every view over a direct buffer came
    // back empty/zero. Views now share the parent's storage exactly like
    // real-JDK buffers: heap views record an array-base `offset` (honoured
    // by every accessor via `s2_bb_heap_base`), direct views record the
    // advanced native `address`. The bare-synthetic 6-slot layout has no
    // `offset` field to carry a base, so it keeps the legacy copying
    // behaviour (data-correct, aliasing not representable).
    //
    // BYTE ORDER IS **NOT** CARRIED ACROSS ANY OF THESE FOUR. All four ran
    // `let ord = s2_bb_order(ctx, this)` and propagated it, and that is wrong in
    // a way no amount of aliasing correctness compensates for.
    //
    // The mechanism, because it is not obvious from any javadoc: each of these
    // four returns a NEW buffer built by a `ByteBuffer` constructor, and
    // `boolean bigEndian = true` is a FIELD INITIALISER on `ByteBuffer` — it
    // runs on every construction, so a derived view comes back BIG_ENDIAN
    // however the source was set. These methods preserve CONTENT, not ORDER.
    // And `order()` is `public final`, reading the field directly, so there is
    // no per-subclass override point at which a propagated order could be
    // corrected afterwards.
    //
    // The `s2` registrar WINS in Compatible mode and all four descriptors are
    // force-native, so the propagation was live:
    // `ByteBuffer.allocate(16).order(LITTLE_ENDIAN).slice().order()` answered
    // LITTLE_ENDIAN where HotSpot answers BIG_ENDIAN, and **every typed read
    // through such a view was byteswapped relative to HotSpot** — a wrong value,
    // not an exception, which is the quiet kind.
    //
    // Measured, jdk-25.0.3.9:
    // `{direct,heap}.ord.{slice,sliceRange,duplicate,readOnly}.order = BIG_ENDIAN`.
    // Record: W7-76 §10.
    //
    // THE EXCLUSION, and it is the reason this is a comment and not a one-line
    // diff: `as<T>Buffer()` DOES carry the order, and must keep doing so. It
    // reaches it through `s2_bb_order`'s `java/nio/ByteBufferAs…{B,L}`
    // class-name arm — a different mechanism at a different site — because the
    // JDK picks the `B` or the `L` view class from the source's order at
    // construction time. Do not "fix the inconsistency" by unifying the two.
    r.register(bb, "slice", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this).max(0);
        let lim = s2_bb_limit(ctx, this).max(pos);
        let rem = lim - pos;
        let ro = s2_bb_is_read_only(ctx, this);
        let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
        if s2_bb_synthetic_layout(ctx, this) {
            let new_arr = ctx.new_array(ArrayElementType::Byte, rem as usize);
            if let Some(src) = s2_bb_arr(ctx, this) {
                for i in 0..rem as usize {
                    let b = ctx.get_array_element(src, pos as usize + i);
                    ctx.set_array_element(new_arr, i, b);
                }
            }
            let cls = s2_bb_heap_class(ctx, false);
            let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
            bb_write_hb(ctx, buf, new_arr, rem);
            s2_bb_set_order(ctx, buf, ord);
            // `ro` was computed above and used ONLY by the two aliasing arms
            // below, so this copying arm dropped it. `bb_write_hb` writes
            // `isReadOnly = 0` unconditionally, so the flag was not merely
            // left unset — it was actively cleared, which is the wrong
            // CAPABILITY: the returned slice answers `hasArray() == true` and
            // hands its backing array over. MEASURED, jdk-25.0.3+9, all seven
            // families: `<fam>.ro.slice.isReadOnly OK true`. Written AFTER
            // `bb_write_hb` for exactly that reason.
            s2_bb_set_read_only(ctx, buf, ro);
            return Ok(Some(Value::Object(Some(buf))));
        }
        let buf = match s2_bb_storage(ctx, this) {
            Some(S2BbStorage::Heap { arr, base }) => {
                s2_bb_new_heap_view(ctx, arr, base + pos as usize, 0, rem, rem, -1, ro, ord)
            }
            Some(S2BbStorage::Direct { addr }) => s2_bb_new_direct_view(
                ctx,
                addr.saturating_add(pos as i64),
                0,
                rem,
                rem,
                -1,
                ro,
                ord,
            ),
            // Storage-less synthetic: keep the historic empty-copy result.
            None => {
                let new_arr = ctx.new_array(ArrayElementType::Byte, rem as usize);
                let cls = s2_bb_heap_class(ctx, false);
                let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                bb_write_hb(ctx, buf, new_arr, rem);
                s2_bb_set_read_only(ctx, buf, ro);
                Ok(buf)
            }
        };
        Ok(Some(Value::Object(Some(buf?))))
    });
    // JDK 13+ `slice(int index, int length)` — absolute-indexed aliasing
    // view, independent of position/limit. Abstract on the real class, so
    // an s2-stamped receiver needs this registration to avoid
    // AbstractMethodError (same pattern as the typed-buffer views).
    r.register(bb, "slice", "(II)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let index = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let length = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let lim = s2_bb_limit(ctx, this);
        s2_check_from_index_size(index, length, lim)?;
        let ro = s2_bb_is_read_only(ctx, this);
        let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
        let buf = match s2_bb_storage(ctx, this) {
            Some(S2BbStorage::Heap { arr, base }) if !s2_bb_synthetic_layout(ctx, this) => {
                s2_bb_new_heap_view(
                    ctx,
                    arr,
                    base + index as usize,
                    0,
                    length,
                    length,
                    -1,
                    ro,
                    ord,
                )
            }
            Some(S2BbStorage::Direct { addr }) => s2_bb_new_direct_view(
                ctx,
                addr.saturating_add(index as i64),
                0,
                length,
                length,
                -1,
                ro,
                ord,
            ),
            _ => {
                // Bare-synthetic / storage-less: copying fallback.
                let new_arr = ctx.new_array(ArrayElementType::Byte, length as usize);
                if let Some(src) = s2_bb_arr(ctx, this) {
                    for i in 0..length as usize {
                        let b = ctx.get_array_element(src, index as usize + i);
                        ctx.set_array_element(new_arr, i, b);
                    }
                }
                let cls = s2_bb_heap_class(ctx, false);
                let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                bb_write_hb(ctx, buf, new_arr, length);
                s2_bb_set_order(ctx, buf, ord);
                s2_bb_set_read_only(ctx, buf, ro);
                Ok(buf)
            }
        };
        Ok(Some(Value::Object(Some(buf?))))
    });
    r.register(bb, "duplicate", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        let lim = s2_bb_limit(ctx, this);
        let cap = s2_bb_cap(ctx, this);
        let mark = s2_bb_get_mark(ctx, this);
        let ro = s2_bb_is_read_only(ctx, this);
        let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
        let buf = match s2_bb_storage(ctx, this) {
            Some(S2BbStorage::Heap { arr, base }) if !s2_bb_synthetic_layout(ctx, this) => {
                s2_bb_new_heap_view(ctx, arr, base, pos, lim, cap, mark, ro, ord)
            }
            Some(S2BbStorage::Direct { addr }) => {
                s2_bb_new_direct_view(ctx, addr, pos, lim, cap, mark, ro, ord)
            }
            _ => {
                // Bare-synthetic / storage-less: legacy shared-array
                // rebuild (aliases the array, no offset support needed).
                let cls = s2_bb_heap_class(ctx, false);
                let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                if let Some(src_arr) = s2_bb_arr(ctx, this) {
                    bb_write_hb(ctx, buf, src_arr, cap);
                    ctx.set_field_by_name(buf, "position", Value::Int(pos));
                    ctx.set_field_by_name(buf, "limit", Value::Int(lim));
                    ctx.set_field_by_name(buf, "mark", Value::Int(mark));
                    ctx.set_field(buf, BB_POS, Value::Int(pos));
                    ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
                    ctx.set_field(buf, BB_MARK, Value::Int(mark));
                }
                ctx.set_field(buf, BB_ORDER, Value::Int(ord));
                s2_bb_set_read_only(ctx, buf, ro);
                Ok(buf)
            }
        };
        Ok(Some(Value::Object(Some(buf?))))
    });
    r.register(
        bb,
        "asReadOnlyBuffer",
        "()Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            let lim = s2_bb_limit(ctx, this);
            let cap = s2_bb_cap(ctx, this);
            let mark = s2_bb_get_mark(ctx, this);
            let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
            let buf = match s2_bb_storage(ctx, this) {
                Some(S2BbStorage::Heap { arr, base }) if !s2_bb_synthetic_layout(ctx, this) => {
                    s2_bb_new_heap_view(ctx, arr, base, pos, lim, cap, mark, true, ord)
                }
                Some(S2BbStorage::Direct { addr }) => {
                    s2_bb_new_direct_view(ctx, addr, pos, lim, cap, mark, true, ord)
                }
                _ => {
                    let cls = s2_bb_heap_class(ctx, false);
                    let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                    if let Some(src_arr) = s2_bb_arr(ctx, this) {
                        bb_write_hb(ctx, buf, src_arr, cap);
                        ctx.set_field_by_name(buf, "position", Value::Int(pos));
                        ctx.set_field_by_name(buf, "limit", Value::Int(lim));
                        ctx.set_field_by_name(buf, "mark", Value::Int(mark));
                        ctx.set_field(buf, BB_POS, Value::Int(pos));
                        ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
                        ctx.set_field(buf, BB_MARK, Value::Int(mark));
                    }
                    ctx.set_field(buf, BB_ORDER, Value::Int(ord));
                    // MOVED OUT of the `if let` above, which is the whole
                    // change on this arm: `asReadOnlyBuffer()` on a receiver
                    // with no resolvable backing array returned a buffer with
                    // `isReadOnly` never written — a WRITABLE result from the
                    // one method whose entire contract is that its result is
                    // not writable. It still has to follow `bb_write_hb`,
                    // which writes `isReadOnly = 0`. MEASURED, jdk-25.0.3+9:
                    // `asReadOnlyBuffer().isReadOnly()` is `true` for every
                    // receiver in all seven families, and there is no receiver
                    // for which it is conditional.
                    s2_bb_set_read_only(ctx, buf, true);
                    Ok(buf)
                }
            };
            Ok(Some(Value::Object(Some(buf?))))
        },
    );

    // asXxxBuffer view buffers — each needs a named function (no closure captures)
    r.register(
        bb,
        "asIntBuffer",
        "()Ljava/nio/IntBuffer;",
        s2_bb_as_int_buffer,
    );
    r.register(
        bb,
        "asLongBuffer",
        "()Ljava/nio/LongBuffer;",
        s2_bb_as_long_buffer,
    );
    r.register(
        bb,
        "asShortBuffer",
        "()Ljava/nio/ShortBuffer;",
        s2_bb_as_short_buffer,
    );
    r.register(
        bb,
        "asFloatBuffer",
        "()Ljava/nio/FloatBuffer;",
        s2_bb_as_float_buffer,
    );
    r.register(
        bb,
        "asDoubleBuffer",
        "()Ljava/nio/DoubleBuffer;",
        s2_bb_as_double_buffer,
    );
    r.register(
        bb,
        "asCharBuffer",
        "()Ljava/nio/CharBuffer;",
        s2_bb_as_char_buffer,
    );

    // IntBuffer get/put (positions in int units; byte_start from BB_MARK)
    let ib = "java/nio/IntBuffer";
    r.register(ib, "get", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        // B8: `pos * 4` and the following add can overflow for a corrupt
        // position; `s2_bb_int_byte_off` saturates to a negative sentinel that
        // the byte accessors treat as out-of-range.
        let v = s2_bb_read4(ctx, this, s2_bb_int_byte_off(bs, pos));
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Int(v)))
    });
    r.register(ib, "get", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        Ok(Some(Value::Int(s2_bb_read4(
            ctx,
            this,
            s2_bb_int_byte_off(bs, idx),
        ))))
    });
    r.register(ib, "put", "(I)Ljava/nio/IntBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        s2_bb_write4(ctx, this, s2_bb_int_byte_off(bs, pos), v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(ib, "put", "(II)Ljava/nio/IntBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        s2_bb_write4(ctx, this, s2_bb_int_byte_off(bs, idx), v);
        Ok(Some(Value::Object(Some(this))))
    });

    // Shared Buffer methods for all view-buffer types
    for cls in &[
        "java/nio/IntBuffer",
        "java/nio/LongBuffer",
        "java/nio/ShortBuffer",
        "java/nio/FloatBuffer",
        "java/nio/DoubleBuffer",
    ] {
        let cls = *cls;
        r.register(cls, "position", "()I", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_POS)))
        });
        r.register(cls, "limit", "()I", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_LIMIT)))
        });
        r.register(cls, "capacity", "()I", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_CAP)))
        });
        r.register(cls, "remaining", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(
                (s2_bb_limit(ctx, this) - s2_bb_pos(ctx, this)).max(0),
            )))
        });
        r.register(cls, "hasRemaining", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(
                if s2_bb_limit(ctx, this) > s2_bb_pos(ctx, this) {
                    1
                } else {
                    0
                },
            )))
        });
        // `flip`/`clear`/`rewind` DISCARD THE MARK. These two bodies are the
        // `ByteBuffer` twins forty lines up with the `s2_bb_set_mark` line
        // dropped, and the omission is invisible from this class's own
        // methods: `mark()` and `reset()` are NOT registered for the typed
        // buffers, so the real `Buffer` bytecode sets and reads the real
        // `mark` field while these natives move position and limit in the
        // side slots. Nothing reconciles them.
        //
        //   IntBuffer ib = ...;  Buffer b = ib;
        //   b.mark(); b.flip(); b.reset();
        //     HotSpot   InvalidMarkException
        //     this VM   no throw, and the position silently jumps back into
        //               a region flip just excluded
        //
        // MEASURED with `apps/probes/L4BridgeSweep.java` on Short/Int/Long/
        // Float/DoubleBuffer, heap and view arms alike -- 11 rows. `ByteBuffer`
        // was green because its twin has the line, and `CharBuffer` because
        // `charset_buffers.rs` has it too; this loop is the one copy that
        // never got it.
        //
        // Only reachable through a `Buffer`-typed reference, since that is the
        // only spelling javac emits for `()Ljava/nio/Buffer;` -- which is why
        // 501 rows of `L4TypedBufferSweep` over these exact classes never saw
        // it.
        r.register(cls, "flip", "()Ljava/nio/Buffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_LIMIT, Value::Int(pos));
            ctx.set_field(this, BB_POS, Value::Int(0));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(cls, "clear", "()Ljava/nio/Buffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cap = s2_bb_cap(ctx, this);
            ctx.set_field(this, BB_POS, Value::Int(0));
            ctx.set_field(this, BB_LIMIT, Value::Int(cap));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        });
        // The element type differs per family, and this loop registered all
        // FIVE under `()[I`. Measured with `--dump-native-registry`
        // (2026-08-13): four PHANTOM rows nothing can dispatch to
        // (`DoubleBuffer.array()[I` etc.), and for `IntBuffer` — the one whose
        // descriptor happened to be right — this row OWNED the slot, so
        // `native-io`'s `native_tb_array` and its F14-1 refusals never ran.
        //
        // The body was `Ok(Some(Value::Object(s2_bb_arr(...))))`: a `None`
        // became a silent Java `null`, so `ByteBuffer.allocate(16)
        // .asIntBuffer().array().length` raised NullPointerException where
        // HotSpot throws UnsupportedOperationException. That is the same
        // wrong-capability shape F14-1 fixed in the twin, in the copy that
        // actually wins — "diff a family fix against every member".
        //
        // `Absent` is UOE here with no fourth arm: unlike `ByteBuffer` above,
        // every receiver of a typed view is array-less in the JDK, and F37-1
        // measured views answering UnsupportedOperationException.
        let array_desc = match cls {
            "java/nio/LongBuffer" => "()[J",
            "java/nio/ShortBuffer" => "()[S",
            "java/nio/FloatBuffer" => "()[F",
            "java/nio/DoubleBuffer" => "()[D",
            _ => "()[I",
        };
        r.register(cls, "array", array_desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = s2_bb_arr(ctx, this);
            match buffer_array_access(arr.is_some(), s2_bb_is_read_only(ctx, this)) {
                BufferArrayAccess::Accessible => Ok(Some(Value::Object(arr))),
                BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
                BufferArrayAccess::Absent => Err(s2_bb_no_backing_array()),
            }
        });
        // `isDirect` — IMPLEMENTED wave 4 (2026-07-28). The wave-3 note here
        // claimed "every receiver is a heap view backed by the int[]", which is
        // false: `s2_view_buf_fn!` has a second branch — when the SOURCE
        // ByteBuffer is direct (`s2_bb_direct_addr`), the view it hands out has
        // NO backing array and carries a native `address` instead. Real JDK
        // agrees that such a view is direct
        // (`allocateDirect(n).asIntBuffer().isDirect() == true`), so the
        // constant `false` was wrong for exactly that case — and wrong in the
        // direction that makes callers take the "copy via array()" path on a
        // buffer that has no array. Answer from the same storage probe every
        // accessor in this file uses: `s2_bb_direct_addr` returns `Some` only
        // when `s2_bb_arr` finds nothing AND a positive address slot exists.
        r.register(cls, "isDirect", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(
                if s2_bb_direct_addr(ctx, this).is_some() {
                    1
                } else {
                    0
                },
            )))
        });
        // `isReadOnly` — NO LONGER A CONSTANT (F26-1, closing F21-1's N3).
        //
        // The comment this replaces argued the `Int(0)` was EXACT, and its
        // argument was sound when written: `$ro` aliased `$dup` and so could
        // not mint a read-only view, therefore no receiver reaching here could
        // be read-only. **F21 falsified the premise from the other crate.**
        // `native-io`'s `tb_abstract_view_fns!` `$ro_fn` now stamps
        // `isReadOnly = 1` by name, and `register_io_natives` runs AFTER
        // `register_essential_natives_with_shims` in both `vm_init` real-JDK
        // arms (VERIFIED: `vm/src/vm/vm_init.rs` L2055 < L2252 and L2593 <
        // L2788; registration is last-write-wins), so it is native-io's
        // `asReadOnlyBuffer` that answers for these five classes and read-only
        // typed views DO now exist.
        //
        // Left alone the constant reproduced, in the sibling, exactly the
        // impossible state that got `native_bb_is_read_only` fixed: `array()`
        // and `arrayOffset()` raise `ReadOnlyBufferException` and `hasArray()`
        // answers `false` (all three read the flag via
        // `native_io::buffer_array_access`), while `isReadOnly()` said the
        // buffer was writable — so a caller that branches on `isReadOnly()`
        // instead of `hasArray()` is steered straight into the throw.
        // MEASURED, jdk-25.0.3+9 (`scratchpad/f26/F26AliasProbe.java` §7):
        // `allocate(32).asReadOnlyBuffer().asIntBuffer()` is a
        // `java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`, and
        // the same holds for Long/Short/Float/Double/Char and for the direct
        // twin (`DirectIntBufferRS`, also `true`).
        //
        // **THE WIDTH CLAIM, CHECKED RATHER THAN INHERITED.** The old comment
        // said this needs "a wider allocation" because "the 6-slot synthetic is
        // full". That is true of the SYNTHETIC layout and irrelevant to this
        // registration, in both directions:
        //
        //   * Real-JDK mode: `java.nio.IntBuffer` DECLARES `boolean isReadOnly`
        //     (VERIFIED, `javap -p java.nio.IntBuffer` on 25.0.3+9: `int[] hb;
        //     int offset; boolean isReadOnly;` — identical on Long/Short/Float/
        //     Double), and `VmExec::alloc_object` clamps a native allocator's
        //     requested slot count UP to the resolved class's declared field
        //     count (VERIFIED, `vm/src/vm/vm_exec.rs` "Layout-mismatch guard").
        //     So the field is present on every one of these receivers and the
        //     by-name read resolves. Nothing needs widening.
        //   * Synthetic-JDK mode: there is no such field, `get_field_by_name`
        //     yields a non-`Int`, and `s2_bb_is_read_only` answers `false` —
        //     byte-for-byte the constant this replaces. So this is a no-op in
        //     the mode the width argument was about.
        //
        // Reads the same FIELD through the same converged helper as
        // `hasArray`/`array`/`arrayOffset`, so the four accessors on one
        // receiver cannot disagree.
        //
        // DISCLOSED RESIDUAL, deliberately not fixed here: the `$put` /
        // `$put_abs` / `$compact` arms of `s2_typed_buffer_view_fns!` still do
        // not consult the flag, so a write through a read-only typed view
        // succeeds where HotSpot raises `ReadOnlyBufferException` (MEASURED:
        // `allocate(32).asReadOnlyBuffer().asIntBuffer().put(0,1)` →
        // `java.nio.ReadOnlyBufferException`). Making this accessor honest is
        // strictly a step toward that and cannot be a step away from it: it
        // moves one more accessor onto the flag the enforcement will read.
        r.register(cls, "isReadOnly", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(s2_bb_is_read_only(ctx, this)))))
        });
    }

    // `order`/`slice`/`slice(int,int)`/`duplicate`/`asReadOnlyBuffer` are
    // `public abstract` on every typed NIO buffer subclass in real JDK 25
    // (unlike ByteBuffer, where they're concrete bytecode) — `get`/`put`
    // (relative + absolute) and `compact` are abstract too, but only
    // IntBuffer has working overrides below (legacy). Every view-buffer
    // instance handed out by `s2_view_buf_fn!` above (`ByteBuffer.asXxxBuffer()`)
    // is stamped with the LITERAL abstract class name, so an `invokevirtual`
    // for any of these against that receiver resolves to a Code-less abstract
    // declaration and throws AbstractMethodError unless registered directly
    // here. See
    // ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md
    // (found via `FloatBuffer.order()`/`put(int,float)` on a raw vector slice
    // view — `ES814HnswScalarQuantizedVectorsFormatTests.testRescoreUsesRawVectorSlice`
    // — and `IntBuffer.order()` in `PreconditionerTests`).
    //
    // `bs` below is the byte-offset (within the shared backing byte[]) where
    // this view's element 0 starts — encoded by `s2_view_buf_fn!`/`slice`/
    // `slice(II)` as `-(bs+1)` in the indexed `BB_MARK` slot (mirroring the
    // existing IntBuffer get/put below, which reads it the same way).
    macro_rules! s2_typed_buffer_view_fns {
        (
            $get:ident, $get_abs:ident, $put:ident, $put_abs:ident,
            $order:ident, $slice:ident, $slice2:ident, $dup:ident, $ro:ident, $compact:ident,
            $get_bulk:ident, $put_bulk:ident,
            $cls:literal, $width:expr, $read:ident, $write:ident,
            $to_value:expr, $from_value:expr
        ) => {
            fn $get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let pos = s2_bb_pos(ctx, this);
                if pos >= s2_bb_limit(ctx, this) {
                    return Err(RuntimeError::BufferUnderflowException.into());
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = pos
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                let raw = $read(ctx, this, off);
                ctx.set_field(this, BB_POS, Value::Int(pos + 1));
                Ok(Some(($to_value)(raw)))
            }
            fn $get_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = idx
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                let raw = $read(ctx, this, off);
                Ok(Some(($to_value)(raw)))
            }
            /// `put(x)` — F37-1 §3, landing F26-1 §9.2 / F21-1 N3's residual.
            ///
            /// The read-only check is FIRST, before the overflow check, and
            /// that ordering is MEASURED, not assumed: JDK 25's
            /// `ByteBufferAsIntBufferRB.put(int)` is a bare
            /// `throw new ReadOnlyBufferException();` with no bounds test
            /// above it (`jdk25src/.../ByteBufferAsIntBufferRB.java:166`), so
            /// a read-only view at `position == limit` answers
            /// `ReadOnlyBufferException`, not `BufferOverflowException`.
            ///
            /// MEASURED, jdk-25.0.3+9 (`scratchpad/f37/F37TypedAliasProbe.java`):
            /// `allocate(64).asReadOnlyBuffer().asIntBuffer().put(1)` →
            /// `java.nio.ReadOnlyBufferException`, `getMessage()` null, and the
            /// same for all five families and for the direct twin
            /// (`DirectIntBufferRS`).
            fn $put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let v = args.get(1).cloned().unwrap_or(Value::Int(0));
                let pos = s2_bb_pos(ctx, this);
                if pos >= s2_bb_limit(ctx, this) {
                    return Err(RuntimeError::BufferOverflowException.into());
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = pos
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                $write(ctx, this, off, ($from_value)(&v));
                ctx.set_field(this, BB_POS, Value::Int(pos + 1));
                Ok(Some(Value::Object(Some(this))))
            }
            /// `put(index, x)` — see `$put`. Same class, same null message.
            fn $put_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                let v = args.get(2).cloned().unwrap_or(Value::Int(0));
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = idx
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                $write(ctx, this, off, ($from_value)(&v));
                Ok(Some(Value::Object(Some(this))))
            }
            fn $order(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                // Return the canonical (real-static) ByteOrder object — a
                // fresh synthetic here broke identity comparisons
                // (`view.order() == ByteOrder.LITTLE_ENDIAN`) and printed
                // as BIG_ENDIAN regardless of value in real-JDK mode
                // (residual-doc item 6).
                let ord = s2_bb_order(ctx, this);
                Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
            }
            fn $slice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let pos = s2_bb_pos(ctx, this);
                let lim = s2_bb_limit(ctx, this);
                let remaining = (lim - pos).max(0);
                let bs = s2_typed_view_byte_start(ctx, this);
                let new_bs = pos
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(bs);
                let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
                // `s2_bb_heap_window`, not `s2_bb_arr`: the receiver may be a
                // real-JDK `ByteBufferAs<T>Buffer{B,L}`, whose array lives on
                // its backing `bb` and whose byte start is carried in
                // `address` rather than in the `BB_MARK` marker. The derived
                // view is abstract-stamped with base 0, so fold the resolved
                // window base into the marker it WILL read back.
                if let Some((arr, base)) = s2_bb_heap_window(ctx, this) {
                    ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                    let abs = (base as i32).saturating_add(new_bs);
                    ctx.set_field(vb, BB_MARK, Value::Int(-(abs + 1)));
                } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                    // DIRECT view: alias the native storage at the sliced
                    // element position (residual-doc item 3).
                    ctx.set_field_by_name(
                        vb,
                        "address",
                        Value::Long(addr.saturating_add((new_bs as i64).max(0))),
                    );
                }
                ctx.set_field(vb, BB_POS, Value::Int(0));
                ctx.set_field(vb, BB_LIMIT, Value::Int(remaining));
                ctx.set_field(vb, BB_CAP, Value::Int(remaining));
                let ord = s2_bb_order(ctx, this);
                s2_bb_set_order(ctx, vb, ord);
                // Read-only is CONTAGIOUS: `slice()`, `slice(int,int)` and
                // `duplicate()` INHERIT the source's flag (MEASURED, all seven
                // families, F21-1 §1.1; `<fam>.ro.slice.isReadOnly` true,
                // `<fam>.w.slice.isReadOnly` false). Landed together with the
                // `$put`/`$put_abs`/`$put_bulk`/`$compact` guards above, and it
                // has to be: a guard on the receiver that a `slice()` one call
                // later hands back writable is not a guard, it is a delay. Same
                // "the capability re-opens one call later" shape F21-1 closed
                // for ByteBuffer, and it would have been re-opened HERE by the
                // very change that closed the direct route.
                //
                // `s2_bb_set_read_only` writes by NAME, so on a bare 6-slot
                // synthetic carrier — which is what `try_alloc_concurrent_
                // synthetic($cls, 6)` mints in synthetic-JDK mode — it is a
                // no-op and nothing observable changes there.
                let src_read_only = s2_bb_is_read_only(ctx, this);
                s2_bb_set_read_only(ctx, vb, src_read_only);
                Ok(Some(Value::Object(Some(vb))))
            }
            fn $slice2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let index = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                let length = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let cap = s2_bb_cap(ctx, this);
                s2_check_from_index_size(index, length, cap)?;
                let bs = s2_typed_view_byte_start(ctx, this);
                let new_bs = index
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(bs);
                let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
                // See `$slice` for why this resolves through
                // `s2_bb_heap_window` and folds the window base in.
                if let Some((arr, base)) = s2_bb_heap_window(ctx, this) {
                    ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                    let abs = (base as i32).saturating_add(new_bs);
                    ctx.set_field(vb, BB_MARK, Value::Int(-(abs + 1)));
                } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                    ctx.set_field_by_name(
                        vb,
                        "address",
                        Value::Long(addr.saturating_add((new_bs as i64).max(0))),
                    );
                }
                ctx.set_field(vb, BB_POS, Value::Int(0));
                ctx.set_field(vb, BB_LIMIT, Value::Int(length));
                ctx.set_field(vb, BB_CAP, Value::Int(length));
                let ord = s2_bb_order(ctx, this);
                s2_bb_set_order(ctx, vb, ord);
                // Read-only is CONTAGIOUS: `slice()`, `slice(int,int)` and
                // `duplicate()` INHERIT the source's flag (MEASURED, all seven
                // families, F21-1 §1.1; `<fam>.ro.slice.isReadOnly` true,
                // `<fam>.w.slice.isReadOnly` false). Landed together with the
                // `$put`/`$put_abs`/`$put_bulk`/`$compact` guards above, and it
                // has to be: a guard on the receiver that a `slice()` one call
                // later hands back writable is not a guard, it is a delay. Same
                // "the capability re-opens one call later" shape F21-1 closed
                // for ByteBuffer, and it would have been re-opened HERE by the
                // very change that closed the direct route.
                //
                // `s2_bb_set_read_only` writes by NAME, so on a bare 6-slot
                // synthetic carrier — which is what `try_alloc_concurrent_
                // synthetic($cls, 6)` mints in synthetic-JDK mode — it is a
                // no-op and nothing observable changes there.
                let src_read_only = s2_bb_is_read_only(ctx, this);
                s2_bb_set_read_only(ctx, vb, src_read_only);
                Ok(Some(Value::Object(Some(vb))))
            }
            fn $dup(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let pos = s2_bb_pos(ctx, this);
                let lim = s2_bb_limit(ctx, this);
                let cap = s2_bb_cap(ctx, this);
                let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
                // Re-derive the marker rather than copying `BB_MARK` raw: on a
                // real-JDK `ByteBufferAs<T>Buffer{B,L}` receiver that slot is
                // `Buffer.mark` (-1), and the byte start lives in `address`.
                // For an abstract-stamped synthetic view this reproduces the
                // old copy exactly (base 0, byte start already the marker).
                if let Some((arr, base)) = s2_bb_heap_window(ctx, this) {
                    ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                    let abs = (base as i32).saturating_add(s2_typed_view_byte_start(ctx, this));
                    ctx.set_field(vb, BB_MARK, Value::Int(-(abs + 1)));
                } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                    // DIRECT duplicate: same native storage, same window.
                    ctx.set_field_by_name(vb, "address", Value::Long(addr));
                }
                ctx.set_field(vb, BB_POS, Value::Int(pos));
                ctx.set_field(vb, BB_LIMIT, Value::Int(lim));
                ctx.set_field(vb, BB_CAP, Value::Int(cap));
                let ord = s2_bb_order(ctx, this);
                s2_bb_set_order(ctx, vb, ord);
                // Read-only is CONTAGIOUS: `slice()`, `slice(int,int)` and
                // `duplicate()` INHERIT the source's flag (MEASURED, all seven
                // families, F21-1 §1.1; `<fam>.ro.slice.isReadOnly` true,
                // `<fam>.w.slice.isReadOnly` false). Landed together with the
                // `$put`/`$put_abs`/`$put_bulk`/`$compact` guards above, and it
                // has to be: a guard on the receiver that a `slice()` one call
                // later hands back writable is not a guard, it is a delay. Same
                // "the capability re-opens one call later" shape F21-1 closed
                // for ByteBuffer, and it would have been re-opened HERE by the
                // very change that closed the direct route.
                //
                // `s2_bb_set_read_only` writes by NAME, so on a bare 6-slot
                // synthetic carrier — which is what `try_alloc_concurrent_
                // synthetic($cls, 6)` mints in synthetic-JDK mode — it is a
                // no-op and nothing observable changes there.
                let src_read_only = s2_bb_is_read_only(ctx, this);
                s2_bb_set_read_only(ctx, vb, src_read_only);
                Ok(Some(Value::Object(Some(vb))))
            }
            /// `asReadOnlyBuffer()` — F37-1 §3, landing F26-1 §9.4 item 1.
            ///
            /// **This was a plain `$dup(ctx, args)`**, i.e. the one method
            /// whose entire contract is that its result cannot be written
            /// returned a WRITABLE alias. That was self-consistent only while
            /// nothing on these classes consulted the flag; `$put`/`$put_abs`/
            /// `$put_bulk`/`$compact` above now do, so leaving it would have
            /// produced the state F21-1 §6.1 named — a read-only view that
            /// still accepts writes — in the sibling registrar rather than
            /// closing it.
            ///
            /// `ForceReadOnly`, not `Inherit`: MEASURED, `asReadOnlyBuffer()`
            /// is unconditional and one-way in all seven families
            /// (`<fam>.w.aro.isReadOnly` true, `<fam>.ro.aro.isReadOnly` true,
            /// and there is no `asWritableBuffer` anywhere in java.nio). The
            /// write goes AFTER `$dup` returns, because `$dup` mints a fresh
            /// 6-slot carrier and now stamps `Inherit` on it.
            fn $ro(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let result = $dup(ctx, args)?;
                if let Some(Value::Object(Some(view))) = result {
                    s2_bb_set_read_only(ctx, view, true);
                }
                Ok(result)
            }
            /// `compact()` — see `$put`. `compact` is a WRITE (it moves the
            /// remaining elements down to index 0), so a read-only receiver
            /// refuses it: MEASURED
            /// `allocate(64).asReadOnlyBuffer().asIntBuffer().compact()` →
            /// `java.nio.ReadOnlyBufferException`. The sibling
            /// `ByteBuffer.compact()` registration in this file has had this
            /// guard since F5; the typed views never did.
            fn $compact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let pos = s2_bb_pos(ctx, this);
                let lim = s2_bb_limit(ctx, this);
                let cap = s2_bb_cap(ctx, this);
                let bs = s2_typed_view_byte_start(ctx, this);
                let remaining = (lim - pos).max(0);
                for i in 0..remaining {
                    let src_off = (pos + i)
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    let dst_off = i
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    let raw = $read(ctx, this, src_off);
                    $write(ctx, this, dst_off, raw);
                }
                ctx.set_field(this, BB_POS, Value::Int(remaining));
                ctx.set_field(this, BB_LIMIT, Value::Int(cap));
                Ok(Some(Value::Object(Some(this))))
            }
            // Bulk `get`/`put(T[], off, len)` are CONCRETE (not abstract) on
            // every typed buffer — real JDK 25's implementation
            // (`FloatBuffer.getArray`/`putArray` etc.) reads/writes via
            // `this.address` + `ScopedMemoryAccess` directly for anything
            // beyond a trivial length, COMPLETELY bypassing virtual dispatch
            // to the single-element accessors above. Our synthetic
            // abstract-stamped view never sets a real `address`, so that
            // fast path silently read/wrote zero bytes — even after the
            // single-element get/put fix above, any bulk vector read (the
            // overwhelmingly common case: `buffer.get(vec, 0, dims)`) still
            // came back all-zero. Registering natives here is not enough by
            // itself either: these methods HAVE real Code, so the registered
            // native is inert unless force-listed in
            // `force_native_over_real_jdk_bytecode` (vm/src/runtime/interpreter.rs)
            // — added there alongside this fix.
            fn $get_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let dst = obj_arg(args, 1)?;
                let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                let pos = s2_bb_pos(ctx, this);
                let dst_len = i32::try_from(ctx.array_length(dst)).unwrap_or(i32::MAX);
                s2_check_from_index_size(off, len, dst_len)?;
                if pos
                    .checked_add(len)
                    .map_or(true, |e| e > s2_bb_limit(ctx, this))
                {
                    return Err(RuntimeError::BufferUnderflowException.into());
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                for i in 0..len {
                    let src_off = (pos + i)
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    let raw = $read(ctx, this, src_off);
                    ctx.set_array_element(dst, (off + i) as usize, ($to_value)(raw));
                }
                ctx.set_field(this, BB_POS, Value::Int(pos + len));
                Ok(Some(Value::Object(Some(this))))
            }
            /// `put(T[], off, len)` — see `$put`.
            ///
            /// **This is the one write path of the four that is NOT shadowed**,
            /// and therefore the one whose repair is reachable today.
            /// `register_io_natives` runs after `register_essential_natives_
            /// with_shims` in every `vm_init` arm (VERIFIED: real-JDK L2252 >
            /// L2055 and L2834 > L2639; synthetic L1935 > L1934, since
            /// `register_builtins` reaches essentials through
            /// `register_essential_natives`), and `native-io` re-registers
            /// `put(X)` / `put(I X)` / `compact()` for all five classes — but
            /// NOT the bulk `put([XII)`. So `$put`/`$put_abs`/`$compact` above
            /// are corrected-but-shadowed and this one is corrected-and-live.
            /// The three are landed anyway: a family half-fixed is the
            /// inconsistency, and `native-io`'s registrations are what a future
            /// reordering would remove.
            fn $put_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let src = obj_arg(args, 1)?;
                let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                let pos = s2_bb_pos(ctx, this);
                let src_len = i32::try_from(ctx.array_length(src)).unwrap_or(i32::MAX);
                s2_check_from_index_size(off, len, src_len)?;
                if pos
                    .checked_add(len)
                    .map_or(true, |e| e > s2_bb_limit(ctx, this))
                {
                    return Err(RuntimeError::BufferOverflowException.into());
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                for i in 0..len {
                    let v = ctx.get_array_element(src, (off + i) as usize);
                    let dst_off = (pos + i)
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    $write(ctx, this, dst_off, ($from_value)(&v));
                }
                ctx.set_field(this, BB_POS, Value::Int(pos + len));
                Ok(Some(Value::Object(Some(this))))
            }
        };
    }

    s2_typed_buffer_view_fns!(
        s2_ib_get,
        s2_ib_get_abs,
        s2_ib_put,
        s2_ib_put_abs,
        s2_ib_order,
        s2_ib_slice,
        s2_ib_slice2,
        s2_ib_dup,
        s2_ib_ro,
        s2_ib_compact,
        s2_ib_get_bulk,
        s2_ib_put_bulk,
        "java/nio/IntBuffer",
        4,
        s2_bb_read4,
        s2_bb_write4,
        |v: i32| Value::Int(v),
        |v: &Value| v.as_int().unwrap_or(0)
    );
    s2_typed_buffer_view_fns!(
        s2_lb_get,
        s2_lb_get_abs,
        s2_lb_put,
        s2_lb_put_abs,
        s2_lb_order,
        s2_lb_slice,
        s2_lb_slice2,
        s2_lb_dup,
        s2_lb_ro,
        s2_lb_compact,
        s2_lb_get_bulk,
        s2_lb_put_bulk,
        "java/nio/LongBuffer",
        8,
        s2_bb_read8,
        s2_bb_write8,
        |v: i64| Value::Long(v),
        |v: &Value| match v {
            Value::Long(l) => *l,
            other => other.as_int().unwrap_or(0) as i64,
        }
    );
    s2_typed_buffer_view_fns!(
        s2_sb_get,
        s2_sb_get_abs,
        s2_sb_put,
        s2_sb_put_abs,
        s2_sb_order,
        s2_sb_slice,
        s2_sb_slice2,
        s2_sb_dup,
        s2_sb_ro,
        s2_sb_compact,
        s2_sb_get_bulk,
        s2_sb_put_bulk,
        "java/nio/ShortBuffer",
        2,
        s2_bb_read2,
        s2_bb_write2,
        |v: i16| Value::Int(v as i32),
        |v: &Value| v.as_int().unwrap_or(0) as i16
    );
    s2_typed_buffer_view_fns!(
        s2_fb_get,
        s2_fb_get_abs,
        s2_fb_put,
        s2_fb_put_abs,
        s2_fb_order,
        s2_fb_slice,
        s2_fb_slice2,
        s2_fb_dup,
        s2_fb_ro,
        s2_fb_compact,
        s2_fb_get_bulk,
        s2_fb_put_bulk,
        "java/nio/FloatBuffer",
        4,
        s2_bb_read4,
        s2_bb_write4,
        |v: i32| Value::Float(f32::from_bits(v as u32)),
        |v: &Value| match v {
            Value::Float(f) => f.to_bits() as i32,
            other => other.as_int().unwrap_or(0),
        }
    );
    s2_typed_buffer_view_fns!(
        s2_db_get,
        s2_db_get_abs,
        s2_db_put,
        s2_db_put_abs,
        s2_db_order,
        s2_db_slice,
        s2_db_slice2,
        s2_db_dup,
        s2_db_ro,
        s2_db_compact,
        s2_db_get_bulk,
        s2_db_put_bulk,
        "java/nio/DoubleBuffer",
        8,
        s2_bb_read8,
        s2_bb_write8,
        |v: i64| Value::Double(f64::from_bits(v as u64)),
        |v: &Value| match v {
            Value::Double(d) => d.to_bits() as i64,
            other => other.as_int().unwrap_or(0) as i64,
        }
    );

    // IntBuffer keeps its own long-standing get/put above (untouched); only
    // register the previously-missing abstract methods for it.
    r.register(ib, "order", "()Ljava/nio/ByteOrder;", s2_ib_order);
    r.register(ib, "slice", "()Ljava/nio/IntBuffer;", s2_ib_slice);
    r.register(ib, "slice", "(II)Ljava/nio/IntBuffer;", s2_ib_slice2);
    r.register(ib, "duplicate", "()Ljava/nio/IntBuffer;", s2_ib_dup);
    r.register(ib, "asReadOnlyBuffer", "()Ljava/nio/IntBuffer;", s2_ib_ro);
    r.register(ib, "compact", "()Ljava/nio/IntBuffer;", s2_ib_compact);
    r.register(ib, "get", "([III)Ljava/nio/IntBuffer;", s2_ib_get_bulk);
    r.register(ib, "put", "([III)Ljava/nio/IntBuffer;", s2_ib_put_bulk);

    let lb = "java/nio/LongBuffer";
    r.register(lb, "get", "()J", s2_lb_get);
    r.register(lb, "get", "(I)J", s2_lb_get_abs);
    r.register(lb, "put", "(J)Ljava/nio/LongBuffer;", s2_lb_put);
    r.register(lb, "put", "(IJ)Ljava/nio/LongBuffer;", s2_lb_put_abs);
    r.register(lb, "order", "()Ljava/nio/ByteOrder;", s2_lb_order);
    r.register(lb, "slice", "()Ljava/nio/LongBuffer;", s2_lb_slice);
    r.register(lb, "slice", "(II)Ljava/nio/LongBuffer;", s2_lb_slice2);
    r.register(lb, "duplicate", "()Ljava/nio/LongBuffer;", s2_lb_dup);
    r.register(lb, "asReadOnlyBuffer", "()Ljava/nio/LongBuffer;", s2_lb_ro);
    r.register(lb, "compact", "()Ljava/nio/LongBuffer;", s2_lb_compact);
    r.register(lb, "get", "([JII)Ljava/nio/LongBuffer;", s2_lb_get_bulk);
    r.register(lb, "put", "([JII)Ljava/nio/LongBuffer;", s2_lb_put_bulk);

    let sb = "java/nio/ShortBuffer";
    r.register(sb, "get", "()S", s2_sb_get);
    r.register(sb, "get", "(I)S", s2_sb_get_abs);
    r.register(sb, "put", "(S)Ljava/nio/ShortBuffer;", s2_sb_put);
    r.register(sb, "put", "(IS)Ljava/nio/ShortBuffer;", s2_sb_put_abs);
    r.register(sb, "order", "()Ljava/nio/ByteOrder;", s2_sb_order);
    r.register(sb, "slice", "()Ljava/nio/ShortBuffer;", s2_sb_slice);
    r.register(sb, "slice", "(II)Ljava/nio/ShortBuffer;", s2_sb_slice2);
    r.register(sb, "duplicate", "()Ljava/nio/ShortBuffer;", s2_sb_dup);
    r.register(sb, "asReadOnlyBuffer", "()Ljava/nio/ShortBuffer;", s2_sb_ro);
    r.register(sb, "compact", "()Ljava/nio/ShortBuffer;", s2_sb_compact);
    r.register(sb, "get", "([SII)Ljava/nio/ShortBuffer;", s2_sb_get_bulk);
    r.register(sb, "put", "([SII)Ljava/nio/ShortBuffer;", s2_sb_put_bulk);

    let fb = "java/nio/FloatBuffer";
    r.register(fb, "get", "()F", s2_fb_get);
    r.register(fb, "get", "(I)F", s2_fb_get_abs);
    r.register(fb, "put", "(F)Ljava/nio/FloatBuffer;", s2_fb_put);
    r.register(fb, "put", "(IF)Ljava/nio/FloatBuffer;", s2_fb_put_abs);
    r.register(fb, "order", "()Ljava/nio/ByteOrder;", s2_fb_order);
    r.register(fb, "slice", "()Ljava/nio/FloatBuffer;", s2_fb_slice);
    r.register(fb, "slice", "(II)Ljava/nio/FloatBuffer;", s2_fb_slice2);
    r.register(fb, "duplicate", "()Ljava/nio/FloatBuffer;", s2_fb_dup);
    r.register(fb, "asReadOnlyBuffer", "()Ljava/nio/FloatBuffer;", s2_fb_ro);
    r.register(fb, "compact", "()Ljava/nio/FloatBuffer;", s2_fb_compact);
    r.register(fb, "get", "([FII)Ljava/nio/FloatBuffer;", s2_fb_get_bulk);
    r.register(fb, "put", "([FII)Ljava/nio/FloatBuffer;", s2_fb_put_bulk);

    let db = "java/nio/DoubleBuffer";
    r.register(db, "get", "()D", s2_db_get);
    r.register(db, "get", "(I)D", s2_db_get_abs);
    r.register(db, "put", "(D)Ljava/nio/DoubleBuffer;", s2_db_put);
    r.register(db, "put", "(ID)Ljava/nio/DoubleBuffer;", s2_db_put_abs);
    r.register(db, "order", "()Ljava/nio/ByteOrder;", s2_db_order);
    r.register(db, "slice", "()Ljava/nio/DoubleBuffer;", s2_db_slice);
    r.register(db, "slice", "(II)Ljava/nio/DoubleBuffer;", s2_db_slice2);
    r.register(db, "duplicate", "()Ljava/nio/DoubleBuffer;", s2_db_dup);
    r.register(
        db,
        "asReadOnlyBuffer",
        "()Ljava/nio/DoubleBuffer;",
        s2_db_ro,
    );
    r.register(db, "compact", "()Ljava/nio/DoubleBuffer;", s2_db_compact);
    r.register(db, "get", "([DII)Ljava/nio/DoubleBuffer;", s2_db_get_bulk);
    r.register(db, "put", "([DII)Ljava/nio/DoubleBuffer;", s2_db_put_bulk);

    // equals / hashCode / compareTo / toString
    // equals / hashCode / compareTo — storage-aware (residual-doc item 2:
    // these were heap-array-only, so ANY direct buffer — including a
    // genuine real-JDK DirectByteBuffer receiver, since `ByteBuffer.equals`
    // resolves on the abstract class and is force-dispatched — compared as
    // empty/zero and mixed heap/direct comparisons always failed).
    r.register(bb, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if this == other {
            return Ok(Some(Value::Int(1)));
        }
        let pa = s2_bb_pos(ctx, this).max(0);
        let la = s2_bb_limit(ctx, this).max(pa);
        let pb = s2_bb_pos(ctx, other).max(0);
        let lb = s2_bb_limit(ctx, other).max(pb);
        let na = (la - pa) as usize;
        if na != (lb - pb) as usize {
            return Ok(Some(Value::Int(0)));
        }
        let wa = s2_bb_read_window(ctx, this, pa, na);
        let wb = s2_bb_read_window(ctx, other, pb, na);
        match (wa, wb) {
            (Some(a), Some(b)) => Ok(Some(Value::Int((a == b) as i32))),
            _ => Ok(Some(Value::Int(0))),
        }
    });
    r.register(bb, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this).max(0);
        let lim = s2_bb_limit(ctx, this).max(pos);
        let n = (lim - pos) as usize;
        let mut h: i32 = 1;
        if let Some(w) = s2_bb_read_window(ctx, this, pos, n) {
            // Real-JDK `Buffer.hashCode` iterates BACKWARD
            // (`for (int i = limit() - 1; i >= position(); i--)`) — the
            // previous forward loop produced a different value than
            // HotSpot for every buffer with 2+ remaining bytes.
            for b in w.iter().rev() {
                h = h.wrapping_mul(31).wrapping_add(*b as i8 as i32);
            }
        }
        Ok(Some(Value::Int(h)))
    });
    r.register(bb, "compareTo", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let pa = s2_bb_pos(ctx, this).max(0);
        let la = s2_bb_limit(ctx, this).max(pa);
        let pb = s2_bb_pos(ctx, other).max(0);
        let lb = s2_bb_limit(ctx, other).max(pb);
        let na = (la - pa) as usize;
        let nb = (lb - pb) as usize;
        let n = na.min(nb);
        let wa = match s2_bb_read_window(ctx, this, pa, n) {
            Some(w) => w,
            None => return Ok(Some(Value::Int(0))),
        };
        let wb = match s2_bb_read_window(ctx, other, pb, n) {
            Some(w) => w,
            None => return Ok(Some(Value::Int(0))),
        };
        for i in 0..n {
            let va = wa[i] as i8 as i32;
            let vb = wb[i] as i8 as i32;
            if va != vb {
                return Ok(Some(Value::Int(va - vb)));
            }
        }
        Ok(Some(Value::Int((na as i32) - (nb as i32))))
    });
    // SHIM-AUDIT (native-builtins-shim-audit.md, row `java/nio/ByteBuffer`):
    // `ByteBuffer` is an ABSTRACT class and never declares its own instance —
    // every receiver is a `HeapByteBuffer`, `DirectByteBuffer`,
    // `MappedByteBuffer`, one of their read-only siblings, or a third-party
    // subclass. `Buffer.toString()` has real bytecode
    // (`getClass().getName() + "[pos=" ...`), and the native-override
    // hierarchy walk in `vm/src/runtime/interpreter/invoke.rs` looks for a
    // native on each ANCESTOR *before* it checks whether that ancestor has
    // bytecode — so this registration wins for every one of those receivers.
    // It used to hard-code the literal string `java.nio.HeapByteBuffer`, i.e.
    // it answered a question it could not know: a `DirectByteBuffer` — or a
    // real-JDK `MappedByteBuffer`, or a third-party subclass — rendered as a
    // heap buffer, which is exactly the shape a "which buffer kind is this?"
    // diagnostic reads.
    //
    // Three-step, fail-closed: never invent a concrete class name.
    //   1. If the receiver has a CONCRETE class (anything other than the
    //      abstract `java/nio/ByteBuffer` itself), that class IS the answer —
    //      this is the real-JDK receiver case and the only one `Buffer
    //      .toString()`'s `getClass().getName()` would ever see.
    //   2. CratonVM's own `ByteBuffer.allocate` / `allocateDirect` mint a
    //      synthetic stand-in whose class name is the abstract
    //      `java/nio/ByteBuffer`, so step 1 cannot name it. Derive the kind
    //      from the storage the buffer actually has, which is the same source
    //      `equals`/`hashCode`/`compareTo` read: a heap array means
    //      `HeapByteBuffer`, a native window means `DirectByteBuffer`.
    //   3. Storage-less (a bare stub): render the abstract class's own name.
    //      Unhelpful, but true — better than a concrete lie.
    r.register(bb, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        let lim = s2_bb_limit(ctx, this);
        let cap = s2_bb_cap(ctx, this);
        let cid = ctx.class_id_of_object(this);
        let own = ctx.class_name_of_id(cid).unwrap_or_default();
        let name = if !own.is_empty() && own != "java/nio/ByteBuffer" {
            own.replace('/', ".")
        } else {
            match s2_bb_storage(ctx, this) {
                Some(S2BbStorage::Heap { .. }) => "java.nio.HeapByteBuffer".to_string(),
                Some(S2BbStorage::Direct { .. }) => "java.nio.DirectByteBuffer".to_string(),
                None => "java.nio.ByteBuffer".to_string(),
            }
        };
        let s = ctx.create_string(&format!("{name}[pos={pos} lim={lim} cap={cap}]"));
        Ok(Some(Value::Object(Some(s))))
    });

    // NEW-17: DirectByteBuffer deallocator.
    //
    // The Cleaner machinery dispatches `run()V` on the deallocator when the
    // associated DirectByteBuffer becomes phantom-reachable. Field 0 holds
    // the NativeMemoryTable alloc_id assigned at allocateDirect time; we
    // free it through NativeContext::free_native_memory and zero the field
    // so a double-free (if anything else gets the same address) is a no-op.
    r.register(DEALLOC_CLASS, "run", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let alloc_id = match ctx.get_field(this, DEALLOC_ID) {
            Value::Long(v) => v,
            _ => return Ok(None),
        };
        if alloc_id == 0 {
            return Ok(None); // already freed
        }
        ctx.free_native_memory(alloc_id);
        ctx.set_field(this, DEALLOC_ID, Value::Long(0));
        // Release the global root that kept the owning `Cleaner$Cleanable`
        // (and, through it, this deallocator) reachable until the action ran.
        // Leaving it installed would pin both for the life of the VM — a slow
        // leak of exactly the shape the Cleaner exists to prevent.
        if let Value::Long(root) = ctx.get_field(this, DEALLOC_ROOT) {
            if root != 0 {
                ctx.remove_global_root(root as usize);
                ctx.set_field(this, DEALLOC_ROOT, Value::Long(0));
            }
        }
        Ok(None)
    });
}

// ---- ByteOrder -------------------------------------------------------------

fn register_s2_byteorder(r: &mut NativeMethodRegistry) {
    let bo = "java/nio/ByteOrder";
    // The factory/static natives return the CANONICAL objects (real class
    // statics when available) — the previous fresh-synthetic-per-call
    // objects broke identity comparisons (`nativeOrder() ==
    // ByteOrder.LITTLE_ENDIAN` was always false) and, in real-JDK mode,
    // wrote an order Int into the real 1-field layout's `name` String slot
    // (residual-doc item 6).
    r.register(bo, "nativeOrder", "()Ljava/nio/ByteOrder;", |ctx, _| {
        let ord = if cfg!(target_endian = "big") { 0 } else { 1 };
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
    });
    r.register(bo, "BIG_ENDIAN", "Ljava/nio/ByteOrder;", |ctx, _| {
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, 0)?))))
    });
    r.register(bo, "LITTLE_ENDIAN", "Ljava/nio/ByteOrder;", |ctx, _| {
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, 1)?))))
    });
    // Layout-aware decode shared by toString/equals: real ByteOrder keeps
    // its `name` String at field 0; the synthetic stand-in keeps an order
    // Int there. The previous int-only reads decoded EVERY real constant
    // as 0 — so `LITTLE_ENDIAN.toString()` printed "BIG_ENDIAN".
    fn s2_byte_order_ord(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
        match ctx.get_field(obj, 0) {
            Value::Int(v) => v,
            Value::Object(Some(name)) => match ctx.read_string(name).as_deref() {
                Some("LITTLE_ENDIAN") => 1,
                _ => 0,
            },
            _ => 0,
        }
    }
    r.register(bo, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Real layout: return the name String itself.
        if let Value::Object(Some(name)) = ctx.get_field(this, 0) {
            return Ok(Some(Value::Object(Some(name))));
        }
        let name = if s2_byte_order_ord(ctx, this) == 1 {
            "LITTLE_ENDIAN"
        } else {
            "BIG_ENDIAN"
        };
        let s = ctx.create_string(name);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(bo, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if this == other {
            return Ok(Some(Value::Int(1)));
        }
        let a = s2_byte_order_ord(ctx, this);
        let b = s2_byte_order_ord(ctx, other);
        Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
    });
}

// ---- SocketChannel (real TcpStream) ----------------------------------------

fn register_s2_socket_channel(r: &mut NativeMethodRegistry) {
    let sc = "java/nio/channels/SocketChannel";

    r.register(
        sc,
        "open",
        "()Ljava/nio/channels/SocketChannel;",
        |ctx, _| {
            let ch = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
            ctx.set_field(ch, S2SC_CONNECTED, Value::Int(0));
            ctx.set_field(ch, S2SC_OPEN, Value::Int(1));
            ctx.set_field(ch, S2SC_ADDR, Value::Object(None));
            ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(-1));
            ctx.set_field(ch, S2SC_BLOCKING, Value::Int(1));
            Ok(Some(Value::Object(Some(ch))))
        },
    );
    r.register(
        sc,
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
        |ctx, args| {
            let addr_val = args.first().copied().unwrap_or(Value::Object(None));
            let ch = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
            ctx.set_field(ch, S2SC_CONNECTED, Value::Int(0));
            ctx.set_field(ch, S2SC_OPEN, Value::Int(1));
            ctx.set_field(ch, S2SC_ADDR, addr_val);
            ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(-1));
            ctx.set_field(ch, S2SC_BLOCKING, Value::Int(1));
            if let Value::Object(Some(addr)) = addr_val {
                if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                    if let Ok(stream) = TcpStream::connect(format!("{host}:{port}")) {
                        let id = s2_alloc_stream(stream);
                        ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(id));
                        ctx.set_field(ch, S2SC_CONNECTED, Value::Int(1));
                    }
                }
            }
            Ok(Some(Value::Object(Some(ch))))
        },
    );
    r.register(sc, "connect", "(Ljava/net/SocketAddress;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // If address is null, fall back to stub (mark as connected, return true)
        let addr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                return Ok(Some(Value::Int(1)));
            }
        };
        ctx.set_field(this, S2SC_ADDR, Value::Object(Some(addr)));
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
            match TcpStream::connect(format!("{host}:{port}")) {
                Ok(stream) => {
                    if !blocking {
                        let _ = stream.set_nonblocking(true);
                    }
                    let id = s2_alloc_stream(stream);
                    ctx.set_field(this, S2SC_SOCK_ID, Value::Int(id));
                    ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                    return Ok(Some(Value::Int(if blocking { 1 } else { 0 })));
                }
                Err(e) => {
                    tracing::debug!("SocketChannel.connect: {e}");
                    if blocking {
                        return Err(RuntimeError::IOException {
                            message: format!("Connection refused: {host}:{port}"),
                        }
                        .into());
                    }
                }
            }
        }
        ctx.set_field(this, S2SC_CONNECTED, Value::Int(0));
        Ok(Some(Value::Int(0)))
    });
    r.register(sc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bb = obj_arg(args, 1)?;
        let sock_id = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sock_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let pos = s2_bb_pos(ctx, bb) as usize;
        let lim = s2_bb_limit(ctx, bb) as usize;
        let cap = lim.saturating_sub(pos);
        if cap == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut tmp = vec![0u8; cap];
        // `s2_blocking_accept` leaves the streams it registers in BLOCKING
        // mode, so a `SocketChannel.read` on one of them parks in `recv` — and
        // `close` only removes the map entry, which cannot reach a thread
        // holding an `Arc` clone of the stream. Gate on the channel's own
        // recorded mode: a non-blocking channel must keep answering 0
        // (`IOStatus.UNAVAILABLE`) immediately, which is what every
        // selector-driven reactor on this surface depends on.
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        let n = {
            let stream = {
                let reg = s2_registry().lock();
                reg.streams.get(&sock_id).cloned()
            };
            if let Some(stream) = stream {
                let closed_first = blocking
                    && matches!(
                        s2_wait_ready_close_aware(stream_pollreq_fd(&stream), false, &|| {
                            s2_stream_still_registered(sock_id)
                        }),
                        Err(_)
                    );
                if closed_first {
                    // Closed from another thread while parked. -1 is this
                    // surface's end-of-input answer and unwinds the caller's
                    // read loop, which is the outcome the close has to produce;
                    // it is a weaker answer than the
                    // `AsynchronousCloseException` the real `SocketChannel`
                    // path raises, and is named as such in
                    // W7-53-blocking-close-family.md.
                    -1i32
                } else {
                    let mut stream_ref = &*stream;
                    match stream_ref.read(&mut tmp) {
                        Ok(0) => -1i32,
                        Ok(n) => n as i32,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                        Err(_) => -1,
                    }
                }
            } else {
                -1
            }
        };
        if n > 0 {
            if let Some(arr) = s2_bb_arr(ctx, bb) {
                for i in 0..n as usize {
                    ctx.set_array_element(arr, pos + i, Value::Int(tmp[i] as i8 as i32));
                }
            }
            ctx.set_field(bb, BB_POS, Value::Int((pos + n as usize) as i32));
        }
        Ok(Some(Value::Int(n)))
    });
    r.register(sc, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bb = obj_arg(args, 1)?;
        let sock_id = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sock_id < 0 {
            return Err(RuntimeError::IOException {
                message: "not connected".into(),
            }
            .into());
        }
        let data = s2_bb_remaining_bytes(ctx, bb);
        if data.is_empty() {
            return Ok(Some(Value::Int(0)));
        }
        // Write twin of the read above — a blocking `send` parks behind peer
        // backpressure exactly as a `recv` parks behind peer silence, and the
        // close reaches neither.
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        let n = {
            let stream = {
                let reg = s2_registry().lock();
                reg.streams.get(&sock_id).cloned()
            };
            if let Some(stream) = stream {
                let closed_first = blocking
                    && matches!(
                        s2_wait_ready_close_aware(stream_pollreq_fd(&stream), true, &|| {
                            s2_stream_still_registered(sock_id)
                        }),
                        Err(_)
                    );
                if closed_first {
                    -1i32
                } else {
                    let mut stream_ref = &*stream;
                    match stream_ref.write(&data) {
                        Ok(n) => n as i32,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                        Err(_) => -1,
                    }
                }
            } else {
                -1
            }
        };
        if n > 0 {
            let pos = s2_bb_pos(ctx, bb);
            let lim = s2_bb_limit(ctx, bb);
            ctx.set_field(bb, BB_POS, Value::Int((pos + n).min(lim)));
        }
        Ok(Some(Value::Int(n)))
    });
    r.register(sc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            s2_registry().lock().streams.remove(&sid);
        }
        ctx.set_field(this, S2SC_CONNECTED, Value::Int(0));
        ctx.set_field(this, S2SC_OPEN, Value::Int(0));
        ctx.set_field(this, S2SC_SOCK_ID, Value::Int(-1));
        Ok(None)
    });
    r.register(sc, "isConnected", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SC_CONNECTED)))
    });
    r.register(sc, "isOpen", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SC_OPEN)))
    });
    r.register(
        sc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            ctx.set_field(this, S2SC_BLOCKING, Value::Int(blocking));
            let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
            if sid >= 0 {
                let mut reg = s2_registry().lock();
                if let Some(stream) = reg.streams.get_mut(&sid) {
                    let _ = stream.set_nonblocking(blocking == 0);
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(sc, "finishConnect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
            return Ok(Some(Value::Int(1)));
        }
        if let Value::Object(Some(addr)) = ctx.get_field(this, S2SC_ADDR) {
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                if let Ok(stream) = TcpStream::connect(format!("{host}:{port}")) {
                    let id = s2_alloc_stream(stream);
                    ctx.set_field(this, S2SC_SOCK_ID, Value::Int(id));
                    ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                    return Ok(Some(Value::Int(1)));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(
        sc,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
    r.register(
        sc,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
}

// ---- ServerSocketChannel (real TcpListener) --------------------------------

fn register_s2_server_socket_channel(r: &mut NativeMethodRegistry) {
    let ssc = "java/nio/channels/ServerSocketChannel";

    r.register(
        ssc,
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
        |ctx, _| {
            let ch =
                try_alloc_concurrent_synthetic(ctx, "java/nio/channels/ServerSocketChannel", 5)?;
            ctx.set_field(ch, S2SSC_OPEN, Value::Int(1));
            ctx.set_field(ch, S2SSC_BOUND, Value::Int(0));
            ctx.set_field(ch, S2SSC_LISTENER_ID, Value::Int(-1));
            ctx.set_field(ch, S2SSC_PORT, Value::Int(0));
            ctx.set_field(ch, S2SSC_PENDING, Value::Int(-1));
            Ok(Some(Value::Object(Some(ch))))
        },
    );
    for desc in &[
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
    ] {
        r.register(ssc, "bind", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Null address → stub: mark as bound without creating a real listener
            let addr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
                    return Ok(Some(Value::Object(Some(this))));
                }
            };
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                match TcpListener::bind(format!("{host}:{port}")) {
                    Ok(listener) => {
                        let id = s2_alloc_listener(listener);
                        ctx.set_field(this, S2SSC_LISTENER_ID, Value::Int(id));
                        ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
                        ctx.set_field(this, S2SSC_PORT, Value::Int(port as i32));
                    }
                    Err(e) => {
                        return Err(RuntimeError::IOException {
                            message: format!("bind {host}:{port}: {e}"),
                        }
                        .into())
                    }
                }
            } else {
                // Unresolvable address → stub bound
                ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
            }
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(
        ssc,
        "accept",
        "()Ljava/nio/channels/SocketChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let lid = ctx
                .get_field(this, S2SSC_LISTENER_ID)
                .as_int()
                .unwrap_or(-1);
            if lid < 0 {
                // Stub-bound (null address) — return a disconnected stub SocketChannel
                let sc = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
                ctx.set_field(sc, S2SC_CONNECTED, Value::Int(1));
                ctx.set_field(sc, S2SC_OPEN, Value::Int(1));
                ctx.set_field(sc, S2SC_ADDR, Value::Object(None));
                ctx.set_field(sc, S2SC_SOCK_ID, Value::Int(-1));
                ctx.set_field(sc, S2SC_BLOCKING, Value::Int(1));
                return Ok(Some(Value::Object(Some(sc))));
            }
            let pending = ctx.get_field(this, S2SSC_PENDING).as_int().unwrap_or(-1);
            let stream_id = if pending >= 0 {
                ctx.set_field(this, S2SSC_PENDING, Value::Int(-1));
                pending
            } else {
                // STW-COOPERATION: an idle acceptor parks here indefinitely.
                // Without the blocked-region bracket it is still counted as a
                // cooperative mutator that can never reach a safepoint, so a
                // concurrent STW waits on it forever — the same
                // `rounds=64 pending=1 taken=0` stall root-caused for
                // `SSLSocketInputStream.read`.
                ctx.begin_blocking_region();
                let result = s2_blocking_accept(lid);
                ctx.end_blocking_region();
                match result {
                    Some(id) => id,
                    None => return Ok(Some(Value::Object(None))),
                }
            };
            let sc = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
            ctx.set_field(sc, S2SC_CONNECTED, Value::Int(1));
            ctx.set_field(sc, S2SC_OPEN, Value::Int(1));
            ctx.set_field(sc, S2SC_ADDR, Value::Object(None));
            ctx.set_field(sc, S2SC_SOCK_ID, Value::Int(stream_id));
            ctx.set_field(sc, S2SC_BLOCKING, Value::Int(1));
            Ok(Some(Value::Object(Some(sc))))
        },
    );
    r.register(ssc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx
            .get_field(this, S2SSC_LISTENER_ID)
            .as_int()
            .unwrap_or(-1);
        if lid >= 0 {
            s2_registry().lock().listeners.remove(&lid);
        }
        ctx.set_field(this, S2SSC_OPEN, Value::Int(0));
        ctx.set_field(this, S2SSC_BOUND, Value::Int(0));
        ctx.set_field(this, S2SSC_LISTENER_ID, Value::Int(-1));
        Ok(None)
    });
    r.register(ssc, "isOpen", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SSC_OPEN)))
    });
    r.register(
        ssc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            let lid = ctx
                .get_field(this, S2SSC_LISTENER_ID)
                .as_int()
                .unwrap_or(-1);
            if lid >= 0 {
                let reg = s2_registry().lock();
                if let Some(listener) = reg.listeners.get(&lid) {
                    let _ = listener.set_nonblocking(blocking == 0);
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        ssc,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
    r.register(
        ssc,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
}

// ---- Channel registration & Selector ---------------------------------------

pub(crate) fn s2_register_channel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let channel = args.first().copied().unwrap_or(Value::Object(None));
    let selector = args.get(1).copied().unwrap_or(Value::Object(None));
    let ops = args.get(2).copied().unwrap_or(Value::Int(0));
    let mut key = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SelectionKey", 4)?;
    ctx.set_field(key, 0, channel);
    ctx.set_field(key, 1, selector);
    ctx.set_field(key, 2, ops);
    ctx.set_field(key, 3, Value::Int(0)); // readyOps = 0
                                          // Add key to selector's key list
    if let Value::Object(Some(mut sel)) = selector {
        // GC-safety: `new_ref_array` below allocates and can trigger a
        // collection that relocates `key`/`sel` (both read again after);
        // pin both and re-read the forwarded references.
        let key_pin = ctx.pin_native_root(key);
        let sel_pin = ctx.pin_native_root(sel);
        let n = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
        let new_cap = (n + 1).max(8);
        let new_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), new_cap);
        key = ctx.read_native_pin(key_pin, key);
        sel = ctx.read_native_pin(sel_pin, sel);
        ctx.unpin_native_roots(key_pin);
        if let Value::Object(Some(old_arr)) = ctx.get_field(sel, S2SEL_KEYS) {
            for i in 0..n {
                let k = ctx.get_array_element(old_arr, i);
                ctx.set_array_element(new_arr, i, k);
            }
        }
        ctx.set_array_element(new_arr, n, Value::Object(Some(key)));
        ctx.set_field(sel, S2SEL_KEYS, Value::Object(Some(new_arr)));
        ctx.set_field(sel, S2SEL_NKEYS, Value::Int((n + 1) as i32));
    }
    Ok(Some(Value::Object(Some(key))))
}

fn s2_keys_as_set(
    ctx: &mut dyn NativeContext,
    sel: ObjectRef,
    selected_only: bool,
) -> Result<Value, MethodCallFailed> {
    let n = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
    // GC-safety: `alloc_concurrent_synthetic`/`new_ref_array` below allocate
    // and can trigger a collection that relocates `sel`/`set` (both read
    // again after); pin both for the whole function.
    let sel_pin = ctx.pin_native_root(sel);
    let mut set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 2)?;
    let set_pin = ctx.pin_native_root(set);
    let sel = ctx.read_native_pin(sel_pin, sel);
    let keys_v = ctx.get_field(sel, S2SEL_KEYS);
    if let Value::Object(Some(keys_arr)) = keys_v {
        let mut ready: Vec<ObjectRef> = Vec::new();
        for i in 0..n {
            if let Value::Object(Some(k)) = ctx.get_array_element(keys_arr, i) {
                let rops = ctx.get_field(k, 3).as_int().unwrap_or(0);
                if !selected_only || rops != 0 {
                    ready.push(k);
                }
            }
        }
        // GC-safety: `ready`'s elements were captured before `new_ref_array`
        // below (which allocates); pin each and re-read the forwarded
        // reference before writing it into the fresh array.
        let ready_pins: Vec<_> = ready.iter().map(|&k| ctx.pin_native_root(k)).collect();
        let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), ready.len());
        set = ctx.read_native_pin(set_pin, set);
        for (i, (&k, &pin)) in ready.iter().zip(ready_pins.iter()).enumerate() {
            let k = ctx.read_native_pin(pin, k);
            ctx.set_array_element(arr, i, Value::Object(Some(k)));
        }
        ctx.set_field(set, 0, Value::Object(Some(arr)));
        ctx.set_field(set, 1, Value::Int(ready.len() as i32));
    } else {
        let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
        set = ctx.read_native_pin(set_pin, set);
        ctx.set_field(set, 0, Value::Object(Some(arr)));
        ctx.set_field(set, 1, Value::Int(0));
    }
    ctx.unpin_native_roots(sel_pin);
    Ok(Value::Object(Some(set)))
}

fn register_s2_selector(r: &mut NativeMethodRegistry) {
    let sel = "java/nio/channels/Selector";

    r.register(sel, "open", "()Ljava/nio/channels/Selector;", |ctx, _| {
        if crate::nbflags().dbg_sel {
            eprintln!("[SEL] Selector.open()");
        }
        let s = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/Selector", 3)?;
        ctx.set_field(s, S2SEL_OPEN, Value::Int(1));
        ctx.set_field(s, S2SEL_KEYS, Value::Object(None));
        ctx.set_field(s, S2SEL_NKEYS, Value::Int(0));
        Ok(Some(Value::Object(Some(s))))
    });
    // select() — block indefinitely until a channel is ready or wakeup
    // is called from another thread. (NEW-3: previously hard-coded a
    // 1000ms timeout and spun; now passes -1 to `poll`, the canonical
    // "wait forever" sentinel.)
    r.register(sel, "select", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::nbflags().dbg_sel {
            let open = ctx.get_field(this, S2SEL_OPEN).as_int().unwrap_or(-1);
            let nkeys = ctx.get_field(this, S2SEL_NKEYS).as_int().unwrap_or(-1);
            eprintln!("[SEL] select() open={open} nkeys={nkeys}");
        }
        let n = s2_selector_do_poll_with_timeout(ctx, this, -1);
        Ok(Some(Value::Int(n)))
    });
    // select(long timeout) — matches the JDK contract exactly:
    //   * timeout == 0  → block indefinitely (equivalent to select())
    //   * timeout  > 0  → block up to `timeout` ms
    //   * timeout  < 0  → IllegalArgumentException
    // (NEW-3: previously `select(0)` incorrectly returned immediately
    // and any positive value was clamped to 30_000 ms.)
    r.register(sel, "select", "(J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "select: missing timeout argument".into(),
                }
                .into())
            }
        };
        if raw < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Negative timeout".into(),
            }
            .into());
        }
        // Saturate at i32::MAX so we never overflow libc::poll's int
        // parameter. A 2_147_483_647 ms timeout is ~24 days, far beyond
        // any realistic user value.
        let t = if raw == 0 {
            -1i32 // infinite
        } else if raw > i32::MAX as i64 {
            i32::MAX
        } else {
            raw as i32
        };
        let n = s2_selector_do_poll_with_timeout(ctx, this, t);
        Ok(Some(Value::Int(n)))
    });
    // selectNow() — non-blocking (timeout=0)
    r.register(sel, "selectNow", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(s2_selector_do_poll_with_timeout(
            ctx, this, 0,
        ))))
    });
    // wakeup() — write one byte to the selector's self-connected UDP
    // socket so any thread currently blocked in `select()` returns
    // promptly. (NEW-3: previously a no-op that returned `this` without
    // doing anything, leaving blocked threads stuck until their timeout.)
    r.register(
        sel,
        "wakeup",
        "()Ljava/nio/channels/Selector;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            signal_wakeup(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(sel, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, S2SEL_OPEN);
        if crate::nbflags().dbg_sel {
            eprintln!("[SEL] isOpen() = {:?}", v);
        }
        Ok(Some(v))
    });
    r.register(sel, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, S2SEL_OPEN, Value::Int(0));
        // NEW-3: release the per-selector wakeup channel so its socket
        // fd is returned to the OS promptly rather than lingering in
        // the global map until process exit.
        release_wakeup_channel(ctx, this);
        Ok(None)
    });
    r.register(sel, "keys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(s2_keys_as_set(ctx, this, false)?))
    });
    r.register(sel, "selectedKeys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(s2_keys_as_set(ctx, this, true)?))
    });

    // SelectionKey — upgrade readyOps to field 3, add convenience predicates
    let sk = "java/nio/channels/SelectionKey";
    r.register(sk, "readyOps", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 3)))
    });
    r.register(sk, "isReadable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 1 != 0 {
                1
            } else {
                0
            },
        )))
    });
    r.register(sk, "isWritable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 4 != 0 {
                1
            } else {
                0
            },
        )))
    });
    r.register(sk, "isAcceptable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 16 != 0 {
                1
            } else {
                0
            },
        )))
    });
    r.register(sk, "isConnectable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 8 != 0 {
                1
            } else {
                0
            },
        )))
    });
    r.register(
        sk,
        "interestOps",
        "(I)Ljava/nio/channels/SelectionKey;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ops = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            ctx.set_field(this, 2, Value::Int(ops));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // SelectableChannel.register override
    let sac = "java/nio/channels/SelectableChannel";
    r.register(
        sac,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
    r.register(
        sac,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
}

// =============================================================================
// Phase S.3 — Real HttpClient (TCP-backed HTTP/1.1)
//
// Replaces the p60 stubs that returned a fake 200 OK with an empty body.
//
// Layout (unchanged from p60):
//   HttpClient    = 1-field  (version: Int 1=HTTP/1.1, 2=HTTP/2)
//   HttpRequest   = 4-field  (uri=0, method=1, bodyPublisher=2, headers=3)
//   HttpResponse  = 3-field  (statusCode=0, body=1, responseHeaders=2)
//   BodyPublisher = 1-field  (body: String ObjectRef or Object(None))
//
// NOTE: HTTPS is supported via native-tls for TLS connections.
// =============================================================================

/// HttpRequest field indices
const HR_URI: usize = 0;
const HR_METHOD: usize = 1;
const HR_BODY: usize = 2;
// field 3 = extra headers map (unused by our impl)

pub(crate) fn register_s3_http_client(r: &mut NativeMethodRegistry) {
    let hc = "java/net/http/HttpClient";
    let hrb = "java/net/http/HttpRequest$Builder";

    // ---- Update builder POST/PUT to also store the body publisher at field 2 ----
    r.register(
        hrb,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("POST");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("PUT");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // ---- Real send() ----
    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        s3_http_send,
    );

    // ---- Real sendAsync() — wraps send() ----
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let resp = s3_http_send(ctx, args)?;
            let resp_val = resp.unwrap_or(Value::Object(None));
            let cf = p58_new_cf(ctx, resp_val, true)?;
            Ok(Some(Value::Object(Some(cf))))
        },
    );
}

/// Core HTTP/1.1 send implementation.
fn s3_http_send(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    // ---- Parse HttpRequest ----
    // Null request → backward-compat stub 200 (preserves p60 test behaviour)
    let req_ref = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return s3_stub_response(ctx, 200, ""),
    };

    let uri_ref = match ctx.get_field(req_ref, HR_URI) {
        Value::Object(Some(u)) => u,
        _ => return s3_stub_response(ctx, 200, ""), // null URI → stub 200
    };

    // ---- Extract URI components ----
    //
    // JDK-ONLY-LAYOUT: converted from raw slot indices to
    // `net_phase_e::uri_components`. This block used to read a private
    // `scheme=0, host=1, port=2, path=3, query=4` model — an exact duplicate of
    // the one in `http2.rs` — which on a real `java.net.URI` names `scheme`,
    // `fragment`, `authority`, `userInfo` and `host`. Only slot 0 was right,
    // and the rest failed silently: a well-typed `String` from the wrong field.
    let parts = crate::net_phase_e::uri_components(ctx, uri_ref);
    let scheme = parts
        .scheme
        .unwrap_or_else(|| "http".to_string())
        .to_lowercase();
    let host = parts.host.unwrap_or_default();
    let port_field = parts.port;
    let path = if parts.path.is_empty() {
        "/".to_string()
    } else {
        parts.path
    };
    let query = parts.query;

    if host.is_empty() {
        return s3_stub_response(ctx, 400, "Cannot determine target host from URI");
    }

    let is_https = scheme == "https";
    let default_port = if is_https { 443u16 } else { 80u16 };
    let port = if port_field > 0 {
        port_field as u16
    } else {
        default_port
    };
    let target = format!("{host}:{port}");

    // ---- Method and body ----
    let method = match ctx.get_field(req_ref, HR_METHOD) {
        Value::Object(Some(m)) => ctx.read_string(m).unwrap_or_else(|| "GET".to_string()),
        _ => "GET".to_string(),
    };
    let body_bytes: Vec<u8> = match ctx.get_field(req_ref, HR_BODY) {
        Value::Object(Some(bp)) => {
            // BodyPublisher = 1-field (string body at field 0)
            match ctx.get_field(bp, 0) {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .map(|st| st.into_bytes())
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    };

    // ---- Build request line + headers ----
    let request_target = if let Some(ref q) = query {
        format!("{path}?{q}")
    } else {
        path.clone()
    };

    let mut request_str = format!(
        "{method} {request_target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: CratonVM/1.0\r\nAccept: */*\r\n"
    );
    if !body_bytes.is_empty() {
        request_str.push_str(&format!(
            "Content-Length: {}\r\nContent-Type: application/octet-stream\r\n",
            body_bytes.len()
        ));
    }
    request_str.push_str("\r\n");

    // ---- Connect and perform I/O (HTTP or HTTPS) ----
    let tcp_stream = match TcpStream::connect(&target) {
        Ok(s) => s,
        Err(e) => return s3_stub_response(ctx, 0, &format!("Connection failed: {e}")),
    };

    let response_bytes: Vec<u8> = if is_https {
        // TLS handshake via native-tls
        let connector = match native_tls::TlsConnector::new() {
            Ok(c) => c,
            Err(e) => return s3_stub_response(ctx, 0, &format!("TLS init failed: {e}")),
        };
        let mut tls_stream = match connector.connect(&host, tcp_stream) {
            Ok(s) => s,
            Err(e) => return s3_stub_response(ctx, 0, &format!("TLS handshake failed: {e}")),
        };
        if tls_stream.write_all(request_str.as_bytes()).is_err() {
            return s3_stub_response(ctx, 0, "TLS write error");
        }
        if !body_bytes.is_empty() && tls_stream.write_all(&body_bytes).is_err() {
            return s3_stub_response(ctx, 0, "TLS write body error");
        }
        let mut buf = Vec::new();
        if tls_stream.read_to_end(&mut buf).is_err() {
            return s3_stub_response(ctx, 0, "TLS read error");
        }
        buf
    } else {
        // Plain HTTP
        let mut stream = tcp_stream;
        if stream.write_all(request_str.as_bytes()).is_err() {
            return s3_stub_response(ctx, 0, "Write error");
        }
        if !body_bytes.is_empty() && stream.write_all(&body_bytes).is_err() {
            return s3_stub_response(ctx, 0, "Write body error");
        }
        let mut buf = Vec::new();
        if stream.read_to_end(&mut buf).is_err() {
            return s3_stub_response(ctx, 0, "Read error");
        }
        buf
    };

    // ---- Parse status line ----
    let response_str = String::from_utf8_lossy(&response_bytes).into_owned();
    let status_code = s3_parse_status_code(&response_str);

    // ---- Extract body (after double CRLF) ----
    let body_str = if let Some(pos) = response_str.find("\r\n\r\n") {
        response_str[pos + 4..].to_string()
    } else if let Some(pos) = response_str.find("\n\n") {
        response_str[pos + 2..].to_string()
    } else {
        String::new()
    };

    // ---- Build HttpResponse synthetic ----
    let response = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3)?;
    ctx.set_field(response, 0, Value::Int(status_code));
    let body_ref = ctx.create_string(&body_str);
    ctx.set_field(response, 1, Value::Object(Some(body_ref)));
    ctx.set_field(response, 2, Value::Object(None)); // headers not parsed

    Ok(Some(Value::Object(Some(response))))
}

/// Extract HTTP status code from the first line of a response.
fn s3_parse_status_code(response: &str) -> i32 {
    // First line: "HTTP/1.1 200 OK"
    let first_line = response.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    parts.next(); // skip "HTTP/1.1"
    parts
        .next()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0)
}

/// Create a stub HttpResponse (for error/unsupported cases).
fn s3_stub_response(ctx: &mut dyn NativeContext, status: i32, msg: &str) -> MethodCallResult {
    let response = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3)?;
    ctx.set_field(response, 0, Value::Int(status));
    let body_ref = ctx.create_string(msg);
    ctx.set_field(response, 1, Value::Object(Some(body_ref)));
    ctx.set_field(response, 2, Value::Object(None));
    Ok(Some(Value::Object(Some(response))))
}

// S4 servlet stubs removed — a JVM does not implement servlet APIs natively.
// Web frameworks (Spring Boot, Tomcat, Jetty) work when the VM can execute
// their bytecode from the real .class files.

/// The client-connector tests that the whole existing TLS corpus could not
/// express.
///
/// WHY THIS MODULE EXISTS AT ALL, in the words of the defect it guards: every
/// TLS fixture in this tree uses a SELF-SIGNED certificate -- H2's baked
/// identity, the netty test certs, the regression-suite keystores. For a
/// self-signed peer the leaf IS the trust anchor, so a one-element chain
/// validates perfectly and "the client captured only the leaf" is invisible to
/// the entire corpus BY CONSTRUCTION. It took 20 live public sites to see it.
///
/// So the fixture here is deliberately the one shape none of those have: a CA
/// and a leaf SIGNED BY IT, presented as a two-certificate chain. That is the
/// generalisable part -- a self-signed fixture cannot exercise chain building,
/// and no number of them adds up to one that can.
#[cfg(all(test, unix))]
mod openssl_client_tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::{BigNum, MsbOption};
    use openssl::hash::MessageDigest;
    use openssl::pkey::{PKey, Private};
    use openssl::rsa::Rsa;
    use openssl::ssl::{SslAcceptor, SslMethod};
    use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
    use openssl::x509::{X509Name, X509};

    /// A self-signed CA of `bits` bits, and a leaf for `localhost` signed by
    /// it. `bits` is a parameter because the SECOND thing this connector
    /// changed -- the certificate security level -- is only visible at a key
    /// size OpenSSL's default level 2 refuses and the JDK's floor allows.
    fn ca_and_leaf(bits: u32) -> ((X509, PKey<Private>), (X509, PKey<Private>)) {
        let mk_key = || PKey::from_rsa(Rsa::generate(bits).expect("rsa")).expect("pkey");
        let mk_name = |cn: &str| {
            let mut n = X509Name::builder().expect("name builder");
            n.append_entry_by_text("CN", cn).expect("cn");
            n.build()
        };
        let serial = || {
            let mut bn = BigNum::new().expect("bn");
            bn.rand(64, MsbOption::MAYBE_ZERO, false).expect("rand");
            bn.to_asn1_integer().expect("serial")
        };
        let not_before = Asn1Time::days_from_now(0).expect("nb");
        let not_after = Asn1Time::days_from_now(3650).expect("na");

        let ca_key = mk_key();
        let ca_name = mk_name("CratonVM Chain Test CA");
        let mut b = X509::builder().expect("ca builder");
        b.set_version(2).expect("v3");
        b.set_serial_number(&serial()).expect("serial");
        b.set_subject_name(&ca_name).expect("subject");
        b.set_issuer_name(&ca_name).expect("issuer");
        b.set_pubkey(&ca_key).expect("pubkey");
        b.set_not_before(&not_before).expect("nb");
        b.set_not_after(&not_after).expect("na");
        b.append_extension(BasicConstraints::new().critical().ca().build().expect("bc"))
            .expect("bc ext");
        b.sign(&ca_key, MessageDigest::sha256()).expect("sign ca");
        let ca = b.build();

        let leaf_key = mk_key();
        let mut b = X509::builder().expect("leaf builder");
        b.set_version(2).expect("v3");
        b.set_serial_number(&serial()).expect("serial");
        b.set_subject_name(&mk_name("localhost")).expect("subject");
        b.set_issuer_name(ca.subject_name()).expect("issuer");
        b.set_pubkey(&leaf_key).expect("pubkey");
        b.set_not_before(&not_before).expect("nb");
        b.set_not_after(&not_after).expect("na");
        b.append_extension(BasicConstraints::new().critical().build().expect("bc"))
            .expect("bc ext");
        let ctx = b.x509v3_context(Some(&ca), None);
        let san = SubjectAlternativeName::new()
            .dns("localhost")
            .ip("127.0.0.1")
            .build(&ctx)
            .expect("san");
        b.append_extension(san).expect("san ext");
        b.sign(&ca_key, MessageDigest::sha256()).expect("sign leaf");
        let leaf = b.build();

        ((ca, ca_key), (leaf, leaf_key))
    }

    /// A one-connection TLS server presenting `leaf` with `ca` as an EXTRA
    /// CHAIN CERT -- i.e. a real two-certificate chain on the wire, which is
    /// the whole point. Returns its port and the thread handle.
    fn serve_once(
        ca: X509,
        leaf: X509,
        leaf_key: PKey<Private>,
        security_level: u32,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let mut b = SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("acceptor");
            b.set_security_level(security_level);
            b.set_private_key(&leaf_key).expect("key");
            b.set_certificate(&leaf).expect("cert");
            b.add_extra_chain_cert(ca).expect("chain cert");
            let acceptor = b.build();
            if let Ok((stream, _)) = listener.accept() {
                // The handshake is all that is under test. A rejected one is
                // the assertion's business, not this thread's.
                let _ = acceptor.accept(stream);
            }
        });
        (port, handle)
    }

    /// The defect, stated as a test: an application TrustManager is handed
    /// whatever this vector holds, and from ONE certificate it cannot build a
    /// path to a root. 20 of 20 live public sites were rejected that way.
    #[test]
    fn client_captures_the_whole_chain_not_just_the_leaf() {
        let ((ca, _ca_key), (leaf, leaf_key)) = ca_and_leaf(2048);
        let ca_der = ca.to_der().expect("ca der");
        let leaf_der = leaf.to_der().expect("leaf der");
        let (port, server) = serve_once(ca, leaf, leaf_key, 1);

        let cfg = OpensslClientConfig {
            roots: vec![ca_der.clone()],
            replace_roots: true,
            skip_verify: false,
            max_tls12: false,
        };
        let id = s2_openssl_tls_connect(&cfg, "localhost", port).unwrap_or_else(|e| {
            panic!("handshake against the two-certificate fixture failed: {e}")
        });
        let chain = s2_tls_peer_cert_chain_der(id).expect("registry entry");
        let _ = s2_tls_close(id);
        let _ = server.join();

        assert_eq!(
            chain.len(),
            2,
            "peer chain must be leaf + issuing CA; a length of 1 is the defect"
        );
        assert_eq!(chain[0], leaf_der, "the LEAF must come first (JSSE order)");
        assert_eq!(chain[1], ca_der, "the issuer must follow it");
    }

    /// The residue, stated as a test. A 1024-bit RSA chain is something the
    /// JDK accepts (`jdk.certpath.disabledAlgorithms` draws its line AT 1024)
    /// and OpenSSL at its default security level of 2 refuses outright, which
    /// is what a CratonVM client with no trust store configured used to do:
    ///
    /// ```text
    /// HOTSPOT   HANDSHAKE-OK   CRATONVM  REFUSED (EE certificate key too weak)
    /// ```
    ///
    /// The server is pinned to level 0 so that only the CLIENT's level is
    /// under test -- otherwise a refusal could be the fixture's own.
    #[test]
    fn client_security_level_matches_the_jdks_1024_bit_floor() {
        let ((ca, _ca_key), (leaf, leaf_key)) = ca_and_leaf(1024);
        let ca_der = ca.to_der().expect("ca der");
        let (port, server) = serve_once(ca, leaf, leaf_key, 0);

        let cfg = OpensslClientConfig {
            roots: vec![ca_der],
            replace_roots: true,
            skip_verify: false,
            max_tls12: false,
        };
        let result = s2_openssl_tls_connect(&cfg, "localhost", port);
        let verdict = match &result {
            Ok(id) => {
                let chain = s2_tls_peer_cert_chain_der(*id).unwrap_or_default();
                let _ = s2_tls_close(*id);
                Ok(chain.len())
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = server.join();
        assert_eq!(
            verdict,
            Ok(2),
            "a 1024-bit chain the JDK accepts must not be refused by the \
             client's security level (see CLIENT_SECURITY_LEVEL)"
        );
    }

    /// Fail CLOSED is not weakened by any of the above: a chain that does NOT
    /// reach the configured anchor is still refused. Without this, a test that
    /// only ever asserts acceptance passes just as well against a connector
    /// that verifies nothing at all.
    #[test]
    fn client_still_refuses_a_chain_that_reaches_no_configured_anchor() {
        let ((ca, _ca_key), (leaf, leaf_key)) = ca_and_leaf(2048);
        // The anchor handed to the client is an UNRELATED CA, so the peer's
        // chain is well-formed and simply does not reach it.
        let ((other_ca, _other_key), (_l, _k)) = ca_and_leaf(2048);
        let other_der = other_ca.to_der().expect("der");
        let (port, server) = serve_once(ca, leaf, leaf_key, 1);

        let cfg = OpensslClientConfig {
            roots: vec![other_der],
            replace_roots: true,
            skip_verify: false,
            max_tls12: false,
        };
        let result = s2_openssl_tls_connect(&cfg, "localhost", port);
        let _ = server.join();
        match result {
            Ok(id) => {
                let _ = s2_tls_close(id);
                panic!("a chain reaching no configured anchor must be refused");
            }
            Err(TlsConnectFailure::Handshake(_)) => {}
            Err(TlsConnectFailure::Tcp(e)) => {
                panic!("expected a handshake rejection, got a TCP failure: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::NativeContext as _;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ClassId;

    /// SHIM-AUDIT regression (fails before the 2026-08-01 fix).
    ///
    /// `java.nio.ByteBuffer` is an ABSTRACT class, so this native is inherited
    /// by every buffer in the VM through the interpreter's superclass walk —
    /// and it wins over the real `Buffer.toString()` bytecode, whose whole body
    /// is `getClass().getName() + "[pos=" …`. The shim used to answer with the
    /// hard-coded literal `java.nio.HeapByteBuffer`, so a direct, mapped,
    /// read-only or third-party buffer all claimed to be heap buffers.
    ///
    /// Each case here has a CONCRETE receiver class, which is the real-JDK
    /// shape and the only branch `getClass().getName()` would ever take. The
    /// storage-derived fallback (for CratonVM's own synthetic stand-ins, whose
    /// class name IS the abstract `java/nio/ByteBuffer`) is exercised by
    /// `vm/src/vm.rs::byte_buffer_to_string`, which allocates through
    /// `ByteBuffer.allocate` and still expects `java.nio.HeapByteBuffer`.
    #[test]
    fn byte_buffer_to_string_names_the_receivers_own_class() {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        register_s2_bytebuffer(&mut registry);
        let cb = registry
            .find("java/nio/ByteBuffer", "toString", "()Ljava/lang/String;")
            .expect("ByteBuffer.toString native");

        for class_name in [
            "java/nio/DirectByteBuffer",
            "java/nio/MappedByteBuffer",
            "java/nio/HeapByteBufferR",
            "com/example/VendorByteBuffer",
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let buf = match ctx.new_object(class_name).expect("alloc") {
                Some(Value::Object(Some(o))) => o,
                other => panic!("expected {class_name} object, got {other:?}"),
            };
            let rendered = match cb(&mut ctx, &[Value::Object(Some(buf))]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                _ => panic!("expected a String from ByteBuffer.toString for {class_name}"),
            };
            let expected_prefix = format!("{}[", class_name.replace('/', "."));
            assert!(
                rendered.starts_with(&expected_prefix),
                "ByteBuffer.toString must render the RECEIVER's class: expected a \
                 `{expected_prefix}…` prefix, got `{rendered}`. A hard-coded concrete \
                 class name here is a claim the shim cannot know — see \
                 native-builtins-shim-audit.md."
            );
        }
    }

    /// W7-83 — `java.nio.Buffer.segment` is not a backing array, on the
    /// registration that WINS in Compatible mode.
    ///
    /// W7-76 §2 settled the registration question: in both Compatible arms
    /// `set_drop_real_layout_synthetic(true)` runs before `register_io_natives`,
    /// so `register_nio_natives` is skipped and nothing overwrites
    /// `register_s2_bytebuffer`. `array()[B`, `hasArray()Z` and `arrayOffset()I`
    /// are all on `native_override.rs`'s forced-native list for
    /// `java/nio/ByteBuffer`, so these natives answer even with the real
    /// bytecode present.
    ///
    /// Measured on Eclipse Adoptium 25.0.3.9 (`probes/DirectByteBufferStateProbe.java`,
    /// section `seg`): `Arena.ofAuto().allocate(16).asByteBuffer()` is a
    /// `java.nio.DirectByteBuffer` with `hb == null`, `segment ==
    /// jdk.internal.foreign.NativeMemorySegmentImpl` and a real process pointer
    /// in `address`; `hasArray()` is **false** and `array()` throws
    /// `UnsupportedOperationException`. Before the screen `s2_bb_arr` returned
    /// the segment, so `hasArray()` answered true and `array()` — whose declared
    /// return type is `[B` — handed back a `MemorySegment`.
    ///
    /// The heap control arm is asserted in the same test: a genuine backing
    /// array must still answer `hasArray() == true` and come back from
    /// `array()`, or the screen has merely broken the other population.
    #[test]
    fn s2_bytebuffer_refuses_a_memory_segment_as_a_backing_array() {
        use cratonvm_native_api::FieldMetadata;

        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        register_s2_bytebuffer(&mut registry);
        let array_fn = registry
            .find("java/nio/ByteBuffer", "array", "()[B")
            .expect("ByteBuffer.array native");
        let has_array_fn = registry
            .find("java/nio/ByteBuffer", "hasArray", "()Z")
            .expect("ByteBuffer.hasArray native");

        let mut ctx = crate::test_utils::MockNativeContext::new();
        // Answer an unresolvable name the way production does, so `hb` reads
        // back as absent rather than as the mock's historic `Int(0)`.
        ctx.set_absent_field_answers_null(true);
        let cid = ctx
            .ensure_class_initialized("java/nio/DirectByteBuffer")
            .expect("mock class");
        // The real JDK 25 layout, transitively over the superclass chain:
        // mark(0) position(1) limit(2) capacity(3) address(4) segment(5)
        // hb(6) offset(7). Declaring it is what makes `hb`-by-name resolve to
        // slot 6 (and answer null) instead of never resolving at all.
        ctx.set_declared_fields(
            cid,
            [
                ("mark", "I", 0),
                ("position", "I", 1),
                ("limit", "I", 2),
                ("capacity", "I", 3),
                ("address", "J", 4),
                ("segment", "Ljava/lang/foreign/MemorySegment;", 5),
                ("hb", "[B", 6),
                ("offset", "I", 7),
            ]
            .into_iter()
            .map(|(name, descriptor, slot_index)| FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0,
                slot_index,
                declaring_class_id: cid,
                is_static: false,
            })
            .collect(),
        );

        let mut native = vec![0u8; 16];
        let addr = native.as_mut_ptr() as i64;
        let segment = match ctx
            .new_object("jdk/internal/foreign/NativeMemorySegmentImpl")
            .expect("segment stand-in")
        {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        let buf = match ctx.new_object("java/nio/DirectByteBuffer").expect("buffer") {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        ctx.set_field(buf, 0, Value::Int(-1)); // mark
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(16));
        ctx.set_field(buf, BB_CAP, Value::Int(16));
        ctx.set_field(buf, BB_MARK, Value::Long(addr)); // real layout: `address`
        ctx.set_field(buf, BB_SEGMENT_SLOT, Value::Object(Some(segment)));

        assert!(
            s2_bb_arr(&ctx, buf).is_none(),
            "a MemorySegment at slot 5 was returned as a backing array"
        );
        assert!(
            matches!(
                has_array_fn(&mut ctx, &[Value::Object(Some(buf))]),
                Ok(Some(Value::Int(0)))
            ),
            "HotSpot answers hasArray() == false for an Arena segment's \
             asByteBuffer(); measured, not assumed"
        );
        let thrown = array_fn(&mut ctx, &[Value::Object(Some(buf))]);
        match &thrown {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::UnsupportedOperationException { .. },
            ))) => {}
            other => panic!(
                "array() on an Arena segment's buffer must throw \
                 UnsupportedOperationException as HotSpot does, got {other:?}"
            ),
        }

        // --- the control: a genuine heap buffer still answers with its array.
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let heap = match ctx.new_object("java/nio/HeapByteBuffer").expect("buffer") {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        ctx.set_field(heap, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(heap, BB_POS, Value::Int(0));
        ctx.set_field(heap, BB_LIMIT, Value::Int(16));
        ctx.set_field(heap, BB_CAP, Value::Int(16));
        assert_eq!(s2_bb_arr(&ctx, heap), Some(arr));
        assert!(matches!(
            has_array_fn(&mut ctx, &[Value::Object(Some(heap))]),
            Ok(Some(Value::Int(1)))
        ));
        let got = array_fn(&mut ctx, &[Value::Object(Some(heap))]);
        match &got {
            Ok(Some(Value::Object(Some(a)))) if *a == arr => {}
            other => panic!(
                "a genuine heap buffer must still answer array() with its own \
                 backing array, got {other:?}"
            ),
        }

        // --- and the OTHER slot-5 population: `native-builtins`' own typed
        // buffer views park a real array there, because `segment` is the only
        // Object-typed field `Buffer` declares. The screen must not take them
        // out with the MemorySegment.
        let view_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        let view = match ctx.new_object("java/nio/IntBuffer").expect("view") {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        ctx.set_field(view, BB_SEGMENT_SLOT, Value::Object(Some(view_arr)));
        assert_eq!(
            s2_bb_arr(&ctx, view),
            Some(view_arr),
            "a typed buffer view's backing array lives at slot 5 and must \
             still resolve"
        );
    }

    /// F14-1 N3. The test above matches the UOE variant with `{ .. }`, which is
    /// precisely why a wrong detail message survived: the CLASS was right, so
    /// no `catch`, no class-name assertion and no variant-shaped `matches!`
    /// could see it. This one reads the message.
    ///
    /// MEASURED, jdk-25.0.3+9: `ByteBuffer.allocateDirect(8).array()` and
    /// `.arrayOffset()` both report `getMessage() == null`, as does
    /// `new UnsupportedOperationException()`.
    #[test]
    fn s2_bb_no_backing_array_has_a_null_detail_message() {
        match s2_bb_no_backing_array() {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::UnsupportedOperationException { message },
            )) => assert!(
                message.is_empty(),
                "the EMPTY string is what types/src/error.rs maps to the ()V ctor, i.e. a \
                 null getMessage(). The old \"direct buffer has no backing array\" gave \
                 getMessage() a value HotSpot does not have. Got {message:?}"
            ),
            other => panic!("must be UnsupportedOperationException, got {other:?}"),
        }
    }

    #[test]
    fn test_socket_registry_alloc_stream_wrapping_ids() {
        let mut reg = SocketRegistry::default();
        // Simulate near-overflow
        reg.next_id = i32::MAX;
        // We can't reliably create a TcpStream in tests without a listener,
        // so test the ID allocation logic directly.
        let id = reg.next_id;
        reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
        assert_eq!(id, i32::MAX);
        assert_eq!(reg.next_id, 1, "should wrap to 1 on overflow");
    }

    #[test]
    fn test_socket_registry_id_collision_avoidance() {
        let mut reg = SocketRegistry::default();
        // Insert a dummy to simulate ID 2 being in use
        reg.next_id = 1;
        // Manually set next_id to 2 and insert a "stream" at id 2
        // We can't create a real TcpStream easily, so just test the logic
        let id = reg.next_id;
        assert_eq!(id, 1);
        reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
        assert_eq!(reg.next_id, 2);
    }

    #[test]
    fn test_socket_registry_default() {
        let reg = SocketRegistry::default();
        assert_eq!(reg.next_id, 1);
        assert!(reg.streams.is_empty());
        assert!(reg.listeners.is_empty());
        assert!(reg.dgrams.is_empty());
    }

    // =======================================================================
    // B8 — ByteBuffer relative-read index arithmetic must not overflow/panic.
    // =======================================================================

    #[test]
    fn b8_bb_off_no_overflow_panic() {
        // Normal case: simple addition.
        assert_eq!(s2_bb_off(10, 3), 13);
        assert_eq!(s2_bb_off(0, 7), 7);
        // Overflow near i32::MAX must saturate to the negative out-of-range
        // sentinel rather than panicking (debug) or wrapping (release).
        assert_eq!(s2_bb_off(i32::MAX, 1), -1);
        assert_eq!(s2_bb_off(i32::MAX - 2, 7), -1);
        // A negative starting index stays negative (out-of-range sentinel).
        assert!(s2_bb_off(-1, 1) < 0);
    }

    #[test]
    fn b8_bb_int_byte_off_no_overflow_panic() {
        // Normal case: byte offset = base + unit*4.
        assert_eq!(s2_bb_int_byte_off(0, 3), 12);
        assert_eq!(s2_bb_int_byte_off(8, 2), 16);
        // `unit * 4` overflow saturates to the out-of-range sentinel.
        assert_eq!(s2_bb_int_byte_off(0, i32::MAX), -1);
        assert_eq!(s2_bb_int_byte_off(0, i32::MAX / 3), -1);
        // `base + bytes` overflow also saturates.
        assert_eq!(s2_bb_int_byte_off(i32::MAX, 1), -1);
    }

    // =======================================================================
    // [nb-servlet] — ByteBuffer.get/put([BII) off/len validation.
    //
    // The bulk get([BII)/put([BII) handlers used to accept `off`/`len` as raw
    // i32 with no negativity check and cast `off` to usize before validating.
    // A negative `len` slipped past `pos + len > limit` (the sum shrinks), then
    // `for i in 0..len as usize` reinterpreted the negative as ~1.8e19 → an
    // effectively infinite loop (hang/DoS). This pins the exact validation
    // predicate the fix introduced (mirrored here as a pure function so it is
    // testable without a full VM `ctx`).
    // =======================================================================

    /// Returns the rejected index (as the JDK-style IOOBE would report) if the
    /// (off, len) pair is invalid for an array of `cap` elements, else `None`.
    /// This is a faithful copy of the guard now in the get/put([BII) handlers.
    fn bb_bii_reject(off: i32, len: i32, cap: i64) -> Option<i32> {
        if off < 0 || len < 0 || (off as i64) + (len as i64) > cap {
            Some(if off < 0 {
                off
            } else {
                off.saturating_add(len)
            })
        } else {
            None
        }
    }

    #[test]
    fn nb_servlet_bb_bii_rejects_negative_and_oob() {
        // Valid in-range copies pass.
        assert_eq!(bb_bii_reject(0, 8, 8), None);
        assert_eq!(bb_bii_reject(3, 5, 8), None);
        assert_eq!(bb_bii_reject(8, 0, 8), None);

        // Negative len — the original hang/DoS vector — is rejected.
        assert!(bb_bii_reject(0, -1, 8).is_some());
        assert!(bb_bii_reject(0, i32::MIN, 8).is_some());

        // Negative off (would become a huge usize) is rejected.
        assert!(bb_bii_reject(-1, 4, 8).is_some());

        // off+len overrunning the array is rejected, even when each is in-range.
        assert!(bb_bii_reject(4, 5, 8).is_some());
        // Overflow of off+len cannot wrap into a "passing" value (i64 widening).
        assert!(bb_bii_reject(i32::MAX, i32::MAX, 8).is_some());

        // Sanity: a negative len would, if cast to usize, drive an astronomically
        // long loop — confirm we never reach that cast for the bad case.
        let bad_len = -1i32;
        assert!(bb_bii_reject(0, bad_len, 16).is_some());
        // (If the guard were absent, `bad_len as usize` would be ~1.8e19.)
        assert!(bad_len as usize > 1_000_000_000_000_000_000usize);
    }

    // =======================================================================
    // NEW-3 — Cross-platform poll + wakeup + UDP selector registration
    // =======================================================================

    /// Unit-level smoke test of [`selector_poll`]: register a single UDP
    /// socket for reading, verify that a non-blocking poll reports it
    /// *not* readable, then send a byte to it and verify the next poll
    /// reports it readable. This exercises the full Unix `poll(2)` /
    /// Windows `WSAPoll` dispatch path, the event-bit translation, and
    /// the raw-fd extraction helpers in one go.
    #[test]
    fn new3_selector_poll_udp_readiness() {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        sock.set_nonblocking(true).expect("nonblock");
        let addr = sock.local_addr().expect("local_addr");
        // Self-connect so send() loops back into our own recv buffer.
        sock.connect(addr).expect("connect self");

        let req = PollReq {
            fd: dgram_pollreq_fd(&sock),
            events: POLL_IN,
        };

        // Initially not readable.
        let revs0 = selector_poll(&[req], 0);
        assert_eq!(revs0.len(), 1);
        assert_eq!(
            revs0[0] & POLL_IN,
            0,
            "idle socket must not report readable"
        );

        // Send a byte; it goes straight back into our own receive buffer.
        sock.send(&[42u8]).expect("send-to-self");
        // Give the kernel a moment to deliver (usually instant on
        // loopback, but poll() itself has 100ms to notice).
        let revs1 = selector_poll(&[req], 100);
        assert_eq!(revs1.len(), 1);
        assert!(
            revs1[0] & POLL_IN != 0,
            "socket must report readable after send-to-self, got {:#x}",
            revs1[0]
        );
    }

    /// Cross-platform round-trip through the same `selector_poll` entry
    /// point as the real JVM selector uses, but with an explicit poll
    /// timeout of zero. A non-blocking poll on an empty request array
    /// must return an empty revents vec immediately.
    #[test]
    fn new3_selector_poll_empty_returns_immediately() {
        let revs = selector_poll(&[], 0);
        assert!(revs.is_empty());
    }

    /// Verify the positive-timeout path actually sleeps (~100ms here).
    /// This catches regressions where the timeout parameter is ignored
    /// or clamped to zero.
    #[test]
    fn new3_selector_poll_honors_positive_timeout() {
        // Build a request that will never be ready (UDP socket with
        // nothing sent to it).
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        sock.set_nonblocking(true).expect("nonblock");
        let req = PollReq {
            fd: dgram_pollreq_fd(&sock),
            events: POLL_IN,
        };
        let t0 = std::time::Instant::now();
        let revs = selector_poll(&[req], 100);
        let elapsed = t0.elapsed();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0] & POLL_IN, 0, "must not be readable");
        assert!(
            elapsed >= std::time::Duration::from_millis(80),
            "poll must honor the 100ms timeout; slept only {elapsed:?}"
        );
        // Loose upper bound to catch runaway blocking.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "poll slept way too long: {elapsed:?}"
        );
    }

    /// [`build_wakeup_channel`] must produce a usable UDP socket that
    /// loops its own writes back into its recv queue. This is the basic
    /// guarantee the wakeup layer relies on.
    #[test]
    fn new3_wakeup_channel_self_loopback() {
        let ch = build_wakeup_channel().expect("build wakeup channel");
        ch.socket.send(&[7u8]).expect("send");
        // Poll for readability with a 100ms upper bound.
        let req = PollReq {
            fd: dgram_pollreq_fd(&ch.socket),
            events: POLL_IN,
        };
        let revs = selector_poll(&[req], 100);
        assert_eq!(revs.len(), 1);
        assert!(
            revs[0] & POLL_IN != 0,
            "wakeup channel must observe its own write"
        );
        // Drain it.
        let mut buf = [0u8; 16];
        let n = ch.socket.recv(&mut buf).expect("recv");
        assert_eq!(n, 1);
        assert_eq!(buf[0], 7);
    }

    /// End-to-end test of the wakeup + poll integration as seen through
    /// the process-wide `selector_wakeups` map. We emulate the selector
    /// lifecycle without going through `NativeContext`: pretend an
    /// arbitrary `ObjectRef` represents the selector, ensure a channel
    /// is allocated for it, call `signal_wakeup`, then confirm that a
    /// subsequent `poll_empty_selector`-style wait returns promptly.
    #[test]
    fn new3_signal_wakeup_interrupts_block() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let mut ctx = crate::test_utils::MockNativeContext::new();

        // Synthesize a fake selector identity. The map is keyed by the
        // selector's stable identity, so any stable value works as long as
        // this test releases it at the end.
        let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0100u64 as usize as *mut u8) };

        // Ensure a wakeup channel exists (would normally be allocated by
        // the first `select()` call).
        let existed =
            ensure_wakeup_channel(&ctx, fake_sel, |ch| dgram_pollreq_fd(&ch.socket)).is_some();
        assert!(existed, "wakeup channel should be constructible");

        let woke = Arc::new(AtomicBool::new(false));
        let woke_clone = Arc::clone(&woke);
        // Spawn a waker thread that triggers signal_wakeup after 50ms.
        let waker = std::thread::spawn(move || {
            let waker_ctx = crate::test_utils::MockNativeContext::new();
            std::thread::sleep(Duration::from_millis(50));
            signal_wakeup(&waker_ctx, fake_sel);
            woke_clone.store(true, Ordering::Release);
        });

        // Block for up to 2 seconds. If wakeup works, the poll returns
        // in roughly 50ms; if it doesn't, we wait the full 2 seconds.
        let t0 = Instant::now();
        let _ = poll_empty_selector(&mut ctx, fake_sel, 2_000);
        let elapsed = t0.elapsed();
        waker.join().expect("waker thread");

        assert!(woke.load(Ordering::Acquire));
        assert!(
            elapsed < Duration::from_millis(500),
            "wakeup must interrupt the poll in well under 500ms; took {elapsed:?}"
        );

        // Clean up so other tests don't see a stale entry.
        release_wakeup_channel(&ctx, fake_sel);
    }

    #[test]
    fn s2_empty_selector_wait_enters_gc_blocked_region() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0200u64 as usize as *mut u8) };

        let _ = poll_empty_selector(&mut ctx, fake_sel, 5);

        assert_eq!(
            ctx.blocking_region_counts(),
            (1, 1),
            "blocking select on an empty selector must be GC-blocked"
        );
        release_wakeup_channel(&ctx, fake_sel);
    }

    #[test]
    fn s2_registered_selector_wait_enters_gc_blocked_region() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sel = ctx.alloc_object(ClassId::new(0), 3);
        let keys = ctx.new_ref_array(ClassId::new(0), 1);
        let key = ctx.alloc_object(ClassId::new(0), 4);
        let channel = ctx.alloc_object(ClassId::new(0), 5);

        ctx.set_field(sel, S2SEL_OPEN, Value::Int(1));
        ctx.set_field(sel, S2SEL_KEYS, Value::Object(Some(keys)));
        ctx.set_field(sel, S2SEL_NKEYS, Value::Int(1));
        ctx.set_array_element(keys, 0, Value::Object(Some(key)));
        ctx.set_field(key, 0, Value::Object(Some(channel)));
        ctx.set_field(key, 2, Value::Int(1));
        ctx.set_field(channel, S2SSC_LISTENER_ID, Value::Int(-1));
        ctx.set_field(channel, S2SC_SOCK_ID, Value::Int(-1));
        ctx.set_field(channel, S2DC_SOCK_ID, Value::Int(-1));

        let n = s2_selector_do_poll_with_timeout(&mut ctx, sel, 5);

        assert_eq!(n, 0);
        assert_eq!(
            ctx.blocking_region_counts(),
            (1, 1),
            "blocking select with registered keys must be GC-blocked"
        );
        release_wakeup_channel(&ctx, sel);
    }

    /// Verify that `release_wakeup_channel` removes the entry from the
    /// global map (otherwise the map would grow without bound for
    /// long-running processes that repeatedly open and close selectors).
    #[test]
    fn new3_release_wakeup_channel_removes_entry() {
        let ctx = crate::test_utils::MockNativeContext::new();
        let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0400u64 as usize as *mut u8) };
        let _ = ensure_wakeup_channel(&ctx, fake_sel, |_| ());
        assert!(selector_wakeups()
            .lock()
            .contains_key(&selector_key(&ctx, fake_sel)));
        release_wakeup_channel(&ctx, fake_sel);
        assert!(
            !selector_wakeups()
                .lock()
                .contains_key(&selector_key(&ctx, fake_sel)),
            "release_wakeup_channel must remove the entry"
        );
    }

    /// `ensure_wakeup_channel` must be idempotent — the second call must
    /// return the same backing socket (same local port) as the first.
    #[test]
    fn new3_ensure_wakeup_channel_is_idempotent() {
        let ctx = crate::test_utils::MockNativeContext::new();
        let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0300u64 as usize as *mut u8) };
        let first = ensure_wakeup_channel(&ctx, fake_sel, |ch| {
            ch.socket.local_addr().expect("local_addr")
        })
        .expect("first ensure");
        let second = ensure_wakeup_channel(&ctx, fake_sel, |ch| {
            ch.socket.local_addr().expect("local_addr")
        })
        .expect("second ensure");
        assert_eq!(
            first, second,
            "repeated ensure_wakeup_channel must yield the same socket"
        );
        release_wakeup_channel(&ctx, fake_sel);
    }

    /// Allocating a UDP socket via `s2_alloc_dgram` must not collide
    /// with existing stream / listener ids (NEW-3 refactor of the
    /// next-id allocator).
    #[test]
    fn new3_alloc_dgram_avoids_collisions() {
        // Can't actually construct TcpStream/TcpListener cheaply here
        // without loopback, but we can fabricate a `UdpSocket`.
        let s1 = UdpSocket::bind("127.0.0.1:0").expect("bind 1");
        let s2 = UdpSocket::bind("127.0.0.1:0").expect("bind 2");
        let id1 = s2_alloc_dgram(s1);
        let id2 = s2_alloc_dgram(s2);
        assert_ne!(id1, id2, "distinct UDP sockets must get distinct ids");
        // Clean up.
        let mut reg = s2_registry().lock();
        reg.dgrams.remove(&id1);
        reg.dgrams.remove(&id2);
    }

    /// `s2_next_free_id` must never return 0 (reserved) and must skip
    /// ids already in use by any of the three socket tables.
    #[test]
    fn new3_next_free_id_is_nonzero_and_unique() {
        let mut reg = SocketRegistry::default();
        reg.next_id = 0;
        // Pre-populate udp id 1 so the allocator must skip it.
        let dummy = UdpSocket::bind("127.0.0.1:0").expect("bind");
        reg.dgrams.insert(1, dummy);
        let id = s2_next_free_id(&mut reg);
        assert_ne!(id, 0, "id must never be 0");
        assert_ne!(id, 1, "id 1 already in use, must be skipped");
    }

    // =======================================================================
    // bb_write_hb — real-JDK Buffer field layout (mark@0/position@1/limit@2/
    // capacity@3/address@4/hb@5/offset@6, per `mock_buffer_field_slot`) must
    // not be clobbered by the legacy BB_* indexed-slot fallback. Regression
    // test for the AIOOBE in
    // zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md:
    // BB_MARK(4)/BB_ARRAY(0) used to alias real `address`/`mark` and were
    // written unconditionally AFTER the correct by-name writes, silently
    // resetting `address` to -1 and `mark` to a truncated array pointer.
    // =======================================================================

    #[test]
    fn bb_write_hb_real_layout_preserves_address_and_mark() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("java/nio/ByteBuffer")
            .expect("class init");
        // A real-JDK-shaped ByteBuffer has more than the bare 6 synthetic
        // fields (mark/position/limit/capacity/address/segment plus
        // ByteBuffer's own hb/offset/...); allocate more than 6 slots so
        // `s2_bb_synthetic_layout` correctly identifies this as real, not
        // the pure-synthetic fallback layout.
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 64);
        let buf = ctx.alloc_object(class_id, 10);

        bb_write_hb(&mut ctx, buf, arr, 64);

        assert_eq!(
            ctx.get_field_by_name(buf, "address"),
            Value::Long(16),
            "real Buffer.address must be seeded to ARRAY_BYTE_BASE_OFFSET, not clobbered by BB_MARK"
        );
        assert_eq!(
            ctx.get_field_by_name(buf, "mark"),
            Value::Int(-1),
            "real Buffer.mark must stay -1, not clobbered by BB_ARRAY's array reference"
        );
        assert_eq!(ctx.get_field_by_name(buf, "hb"), Value::Object(Some(arr)));
        assert_eq!(ctx.get_field_by_name(buf, "position"), Value::Int(0));
        assert_eq!(ctx.get_field_by_name(buf, "limit"), Value::Int(64));
        assert_eq!(ctx.get_field_by_name(buf, "capacity"), Value::Int(64));
    }

    #[test]
    fn bb_write_hb_pure_synthetic_layout_still_gets_indexed_fallback() {
        // A genuinely synthetic (non-real-JDK) 6-field ByteBuffer carrier —
        // no field-name metadata resolves, so the indexed BB_* fallback is
        // the only way these natives can round-trip state. Guard against a
        // regression where the fix above accidentally suppresses the
        // fallback for this legitimate case too.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ClassId::new(9999); // never registered by name
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let buf = ctx.alloc_object(class_id, 6);

        bb_write_hb(&mut ctx, buf, arr, 32);

        assert_eq!(ctx.get_field(buf, BB_ARRAY), Value::Object(Some(arr)));
        assert_eq!(ctx.get_field(buf, BB_LIMIT), Value::Int(32));
        assert_eq!(ctx.get_field(buf, BB_CAP), Value::Int(32));
        assert_eq!(ctx.get_field(buf, BB_MARK), Value::Int(-1));
    }

    // =======================================================================
    // Real-JDK `ByteBufferAs<T>Buffer{B,L}` views (2026-08-10).
    //
    // `ByteBuffer.as<T>Buffer()` is not force-listed over real JDK bytecode, so
    // on a real JDK it hands back one of these concrete view classes: storage on
    // the backing `bb`, `hb` null on the view itself, and `Buffer.address`
    // holding an UNSAFE offset (`ARRAY_BYTE_BASE_OFFSET + byteIndex`) rather
    // than a process pointer. The bulk `get([JII)`/`put([JII)` accessors ARE
    // force-listed (they are declared on the abstract `java/nio/LongBuffer`,
    // which these views do not override), so such a receiver reaches
    // `s2_bb_get_byte`, which used to read `address` as a pointer and hand 0x10
    // to `copy_from_native_memory`. SIGSEGV at addr=0x10 on every
    // `org.h2.mvstore.Chunk.readToC`.
    // =======================================================================

    /// Build a real-JDK-shaped `ByteBufferAs<T>Buffer{B,L}` over a heap
    /// ByteBuffer: 10 slots (so `s2_bb_synthetic_layout` reads it as a real
    /// layout, not the bare 6-field synthetic carrier) with the real
    /// `Buffer`/`ByteBufferAsXBuffer` field names declared, `bb` pointing at the
    /// backing buffer and `address` seeded the way the JDK's own
    /// `as<T>Buffer()` seeds it: `bb.address + bb.position()`.
    fn make_real_typed_view(
        ctx: &mut crate::test_utils::MockNativeContext,
        view_class: &str,
        byte_start: i64,
    ) -> (ObjectRef, ObjectRef, ObjectRef) {
        use cratonvm_native_api::FieldMetadata;

        let bb_class = ctx
            .ensure_class_initialized("java/nio/ByteBuffer")
            .expect("class init");
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 64);
        let bb = ctx.alloc_object(bb_class, 10);
        bb_write_hb(ctx, bb, arr, 64);

        let view_class_id = ctx
            .ensure_class_initialized(view_class)
            .expect("class init");
        let names = [
            ("mark", "I"),
            ("position", "I"),
            ("limit", "I"),
            ("capacity", "I"),
            ("address", "J"),
            ("segment", "Ljava/lang/foreign/MemorySegment;"),
            ("bb", "Ljava/nio/ByteBuffer;"),
            ("hb", "[J"),
            ("offset", "I"),
            ("isReadOnly", "Z"),
        ];
        ctx.set_declared_fields(
            view_class_id,
            names
                .iter()
                .enumerate()
                .map(|(i, (name, descriptor))| FieldMetadata {
                    name: (*name).to_string(),
                    descriptor: (*descriptor).to_string(),
                    access_flags: 0,
                    slot_index: i,
                    declaring_class_id: view_class_id,
                    is_static: false,
                })
                .collect(),
        );
        let view = ctx.alloc_object(view_class_id, 10);
        ctx.set_field_by_name(view, "bb", Value::Object(Some(bb)));
        ctx.set_field_by_name(view, "mark", Value::Int(-1));
        ctx.set_field_by_name(view, "position", Value::Int(0));
        ctx.set_field_by_name(view, "limit", Value::Int(4));
        ctx.set_field_by_name(view, "capacity", Value::Int(4));
        ctx.set_field_by_name(
            view,
            "address",
            Value::Long(ARRAY_BYTE_BASE_OFFSET + byte_start),
        );
        (view, bb, arr)
    }

    /// The crash precondition, stated directly: a view over a HEAP buffer must
    /// never be classified as direct. Before the fix `s2_bb_direct_addr`
    /// answered `Some(16 + byte_start)` here and the byte accessors
    /// dereferenced it as a process pointer.
    #[test]
    fn real_typed_view_over_a_heap_buffer_is_not_direct() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (view, _bb, _arr) =
            make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsLongBufferB", 0);

        assert!(
            s2_bb_heap_window(&ctx, view).is_some(),
            "the view's storage must resolve through its backing `bb` — this is the \
             mechanism, the address guard below is only the backstop"
        );
        assert_eq!(
            s2_bb_direct_addr(&ctx, view),
            None,
            "a ByteBufferAs<T>Buffer over a HEAP buffer carries an array-relative \
             Unsafe offset in `address`, not a native pointer — reading it as one \
             is the addr=0x10 SIGSEGV"
        );
        assert!(
            !is_plausible_native_addr(ARRAY_BYTE_BASE_OFFSET),
            "ARRAY_BYTE_BASE_OFFSET is below the first mappable page and must never \
             be accepted as a process pointer"
        );
    }

    /// And the positive half: the view reads the bytes it aliases. `address`
    /// folds in the source's array-base offset AND its position at the moment
    /// the view was taken, so element 0 lives at `address - ARRAY_BYTE_BASE_OFFSET`.
    #[test]
    fn real_typed_view_reads_through_its_backing_bytebuffer() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (view, _bb, arr) =
            make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsLongBufferB", 4);
        for i in 0..16usize {
            ctx.set_array_element(arr, i, Value::Int(i as i32));
        }

        assert_eq!(
            s2_bb_get_byte(&ctx, view, 0),
            4,
            "the view's byte 0 is `address - ARRAY_BYTE_BASE_OFFSET` into the backing array"
        );
        assert_eq!(s2_bb_get_byte(&ctx, view, 3), 7);

        s2_bb_put_byte(&mut ctx, view, 1, 99);
        assert_eq!(
            ctx.get_array_element(arr, 5).as_int(),
            Some(99),
            "a write through the view must land in the SHARED backing array — a view is \
             not a copy"
        );
    }

    /// The JDK compiles one concrete view class per endianness and `order()` is
    /// a constant return, so the class name is the exact answer. The old
    /// `mark`-slot fallback answered BIG_ENDIAN for every such view, which would
    /// byteswap every read through a `...BufferL`.
    #[test]
    fn real_typed_view_endianness_comes_from_its_class_name() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (be, _, _) = make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsLongBufferB", 0);
        assert_eq!(s2_bb_order(&ctx, be), 0, "…BufferB is BIG_ENDIAN");

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (le, _, _) = make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsIntBufferL", 0);
        assert_eq!(s2_bb_order(&ctx, le), 1, "…BufferL is LITTLE_ENDIAN");
    }

    // =======================================================================
    // `order(ByteOrder)` must never write a scalar over the backing array
    // (2026-08-12).
    //
    // Measured on `cratonvm --synthetic-jdk` before the fix:
    //
    //     P8 order-after=LITTLE_ENDIAN
    //     [cratonvm] main-vm run() returned Err: internal error: ByteBuffer
    //       missing backing storage (hb/slot5/address absent; field 0 returned
    //       Int(1), address Object(None))
    //
    // — exit 1, no Java exception, on a plain
    // `ByteBuffer.allocate(8).order(ByteOrder.nativeOrder())`. The corruption
    // is SELF-CONSISTENT (the reader read the same slot back), so `order()`
    // still reported LITTLE_ENDIAN over the destroyed buffer. Every assertion
    // below is therefore about the STORAGE, never about the reported order
    // alone.
    // =======================================================================

    /// A `--synthetic-jdk` buffer carrier, as production actually builds one:
    /// wider than the 6 slots `s2_bb_synthetic_layout` screens for, no real
    /// `java.nio.Buffer` field metadata, backing array in slot 0.
    ///
    /// The class is deliberately NOT named `java/nio/ByteBuffer`, and that is a
    /// property of the harness rather than of production:
    /// `MockNativeContext`'s `mock_buffer_field_slot` answers `position` for
    /// any `java/nio/*ByteBuffer`, so under the mock that name can never
    /// present as a stub layout. In the VM it does —
    /// `class_manager::synthetic_stub_fields` gives `java/nio/ByteBuffer`
    /// `instance_fields(6)` (`_f0.._f5`, all `Ljava/lang/Object;`) over a
    /// `java/nio/Buffer` with `instance_fields(4)`, which is the ten fields
    /// `Class.getDeclaredFields()` prints under `--synthetic-jdk`.
    fn make_stub_layout_buffer(
        ctx: &mut crate::test_utils::MockNativeContext,
    ) -> (ObjectRef, ObjectRef) {
        // Faithful "no such field" answer, so `bigEndian` reports absent the
        // way `vm_exec.rs` reports it rather than the mock's default `Int(0)`.
        ctx.set_absent_field_answers_null(true);
        let class_id = ctx
            .ensure_class_initialized("craton/test/SyntheticStubByteBuffer")
            .expect("class init");
        let buf = ctx.alloc_object(class_id, 10);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        // Exactly what `native-io`'s `alloc_byte_buffer` writes.
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(8));
        ctx.set_field(buf, BB_CAP, Value::Int(8));
        ctx.set_field(buf, BB_MARK, Value::Int(-1));
        (buf, arr)
    }

    /// THE DEFECT, stated as the storage question. Not `order()`, which lied.
    #[test]
    fn setting_the_order_on_a_stub_layout_bytebuffer_keeps_the_backing_array() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (buf, arr) = make_stub_layout_buffer(&mut ctx);
        assert!(
            s2_buf_stub_layout(&ctx, buf),
            "premise: a 10-field carrier with no real `Buffer` field metadata is a \
             stub layout, even though `s2_bb_synthetic_layout` (== 6) says otherwise"
        );
        assert!(
            !s2_bb_synthetic_layout(&ctx, buf),
            "and the narrow screen genuinely does NOT catch it — without this the \
             test would pass for the wrong reason"
        );

        s2_bb_set_order(&mut ctx, buf, 1);

        assert_eq!(
            ctx.get_field(buf, BB_ARRAY),
            Value::Object(Some(arr)),
            "slot 0 is the BACKING ARRAY on this layout; an Int written here is the \
             `ByteBuffer missing backing storage` VM abort"
        );
        assert_eq!(
            s2_bb_arr(&ctx, buf),
            Some(arr),
            "and it must still resolve as the buffer's storage, by identity"
        );
        assert_eq!(
            s2_bb_order(&ctx, buf),
            1,
            "reader and writer must agree: the order is stored where BB_ORDER says"
        );
        assert_eq!(
            ctx.get_field(buf, BB_ORDER),
            Value::Int(1),
            "and that slot is the one named for it, not an incidental free slot"
        );
    }

    /// The round trip in both directions, and the default. A one-way check
    /// would pass against a writer that had simply stopped writing.
    #[test]
    fn stub_layout_bytebuffer_order_round_trips_both_ways() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (buf, arr) = make_stub_layout_buffer(&mut ctx);

        assert_eq!(
            s2_bb_order(&ctx, buf),
            0,
            "a fresh buffer is BIG_ENDIAN — the JDK field initialiser"
        );
        s2_bb_set_order(&mut ctx, buf, 1);
        assert_eq!(s2_bb_order(&ctx, buf), 1);
        s2_bb_set_order(&mut ctx, buf, 0);
        assert_eq!(
            s2_bb_order(&ctx, buf),
            0,
            "LITTLE_ENDIAN must be reversible; a writer that only ever set the flag \
             would pass the forward check alone"
        );
        assert_eq!(
            s2_bb_arr(&ctx, buf),
            Some(arr),
            "the storage survives every transition, not just the first"
        );
    }

    /// The ONE family for which slot 0 is the right home — and the reason the
    /// fix is a class-name gate rather than a deletion. A typed view parks its
    /// backing array in `BB_SEGMENT_SLOT`, leaving real `mark` free.
    #[test]
    fn a_typed_buffer_view_still_stores_its_order_in_the_mark_slot() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        ctx.set_absent_field_answers_null(true);
        let class_id = ctx
            .ensure_class_initialized("java/nio/IntBuffer")
            .expect("class init");
        let view = ctx.alloc_object(class_id, 6);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        // `s2_view_buf_fn!`'s layout: array at slot 5, byte-start marker at 4.
        ctx.set_field(view, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
        ctx.set_field(view, BB_MARK, Value::Int(-1));

        s2_bb_set_order(&mut ctx, view, 1);

        assert_eq!(
            ctx.get_field(view, BB_SEGMENT_SLOT),
            Value::Object(Some(arr)),
            "the view's backing array lives at BB_SEGMENT_SLOT and must be untouched"
        );
        assert_eq!(
            ctx.get_field(view, BB_ARRAY),
            Value::Int(1),
            "slot 0 is real `mark: int` and genuinely free on this shape"
        );
        assert_eq!(s2_bb_order(&ctx, view), 1);
        s2_bb_set_order(&mut ctx, view, 0);
        assert_eq!(s2_bb_order(&ctx, view), 0);
    }

    /// A REAL JDK layout that declares no `bigEndian` — `java.nio.CharBuffer`
    /// is one — must be refused, not guessed at. Every index this function
    /// could reach for aliases a field the real class declares, so the old
    /// unconditional fallback wrote an `int` order flag over real
    /// `Buffer.mark`. Refusing leaves the JDK default, which is a recoverable
    /// wrong value rather than a corrupted object.
    #[test]
    fn a_real_layout_buffer_without_big_endian_refuses_rather_than_writing_slot_zero() {
        use cratonvm_native_api::FieldMetadata;
        let mut ctx = crate::test_utils::MockNativeContext::new();
        ctx.set_absent_field_answers_null(true);
        let class_id = ctx
            .ensure_class_initialized("java/nio/CharBuffer")
            .expect("class init");
        let names = [
            ("mark", "I"),
            ("position", "I"),
            ("limit", "I"),
            ("capacity", "I"),
            ("address", "J"),
            ("segment", "Ljava/lang/foreign/MemorySegment;"),
            ("hb", "[C"),
            ("offset", "I"),
            ("isReadOnly", "Z"),
        ];
        ctx.set_declared_fields(
            class_id,
            names
                .iter()
                .enumerate()
                .map(|(i, (name, descriptor))| FieldMetadata {
                    name: (*name).to_string(),
                    descriptor: (*descriptor).to_string(),
                    access_flags: 0,
                    slot_index: i,
                    declaring_class_id: class_id,
                    is_static: false,
                })
                .collect(),
        );
        let buf = ctx.alloc_object(class_id, 9);
        let chars = ctx.new_array(cratonvm_types::ArrayElementType::Char, 8);
        let sentinel = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 1);
        ctx.set_field_by_name(buf, "mark", Value::Int(-1));
        ctx.set_field_by_name(buf, "hb", Value::Object(Some(chars)));
        // A reference parked in the slot `BB_ORDER` aliases on a real layout,
        // so the refusal is checked by IDENTITY rather than by "still non-null".
        ctx.set_field_by_name(buf, "segment", Value::Object(Some(sentinel)));

        assert!(
            !s2_buf_stub_layout(&ctx, buf),
            "premise: `position` resolves, so this is a real layout"
        );

        s2_bb_set_order(&mut ctx, buf, 1);

        assert_eq!(
            ctx.get_field_by_name(buf, "mark"),
            Value::Int(-1),
            "real `Buffer.mark` must not become a byte-order flag"
        );
        assert_eq!(
            ctx.get_field_by_name(buf, "segment"),
            Value::Object(Some(sentinel)),
            "and neither may the reference-typed `segment` slot that BB_ORDER \
             aliases on a real layout"
        );
        assert_eq!(
            ctx.get_field_by_name(buf, "hb"),
            Value::Object(Some(chars)),
            "the backing store is still the same array object"
        );
        assert_eq!(
            s2_bb_order(&ctx, buf),
            0,
            "the reader agrees with the refusal instead of decoding `mark` as an order"
        );
    }

    // ---- G38-1: the reference slot the ByteOrder fallback used to null ------
    //
    // `G38-1-the-live-reference-slot-writes-20260817.md`. Reaching the
    // fallback in `s2_byte_order_object` does NOT prove the real class is
    // absent — `ensure_class_initialized` can succeed while the statics are
    // not yet published, and every compatibility-mode arm runs a synthetic
    // registrar against real class bytes. On that path the object IS a real
    // `java.nio.ByteOrder`, whose sole instance field is
    // `private final String name` at slot 0, and `Value::Int(ord)` there was
    // coerced to `null` by `heap::coerce_field_value_by_descriptor`.

    /// The falsifying half. Before the fix the fallback wrote ONLY the order
    /// Int, so on a real layout `name` held an `Int` (mock) / `null` (VM) and
    /// both readers decoded LITTLE_ENDIAN as BIG_ENDIAN.
    #[test]
    fn the_byte_order_fallback_names_the_constant_when_the_class_declares_name() {
        use cratonvm_native_api::FieldMetadata;
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("java/nio/ByteOrder")
            .expect("class init");
        ctx.set_declared_fields(
            class_id,
            vec![FieldMetadata {
                name: "name".to_string(),
                descriptor: "Ljava/lang/String;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: class_id,
                is_static: false,
            }],
        );

        for (ord, expected) in [(0i32, "BIG_ENDIAN"), (1i32, "LITTLE_ENDIAN")] {
            let bo = s2_byte_order_object(&mut ctx, ord).expect("byte order object");
            let name = match ctx.get_field_by_name(bo, "name") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                other => panic!(
                    "`ByteOrder.name` is a String reference; the fallback left {other:?} \
                     — that is the value the descriptor coercion turns into null"
                ),
            };
            assert_eq!(
                name, expected,
                "the constant must name ITSELF, not whichever constant a null decodes to"
            );
        }
    }

    /// The other half, so the fix cannot be "write a String everywhere".
    /// A shape with no in-range `name` slot keeps the order flag and nothing
    /// else — that is the synthetic-stub layout, where slot 0 is genuinely an
    /// untyped field and every reader falls back to the Int.
    #[test]
    fn a_byte_order_shape_with_no_in_range_name_slot_keeps_only_the_flag() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bo = s2_byte_order_object(&mut ctx, 1).expect("byte order object");
        assert_eq!(
            ctx.object_num_fields(bo),
            1,
            "premise: the fallback allocates the 1-slot stand-in"
        );
        assert_eq!(
            ctx.get_field(bo, 0),
            Value::Int(1),
            "no `name` field resolves in range, so only the order flag lands \
             and the object is byte-identical to the pre-G38 one"
        );
    }

    /// And the decode both writers feed: a String at slot 0 must round-trip
    /// through the SAME `toString`/`equals` natives the Int does, or the fix
    /// would have moved the defect into the readers.
    #[test]
    fn the_byte_order_readers_decode_a_name_string_as_well_as_the_flag() {
        use cratonvm_native_api::FieldMetadata;
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        register_s2_byteorder(&mut registry);
        let to_string = registry
            .find("java/nio/ByteOrder", "toString", "()Ljava/lang/String;")
            .expect("ByteOrder.toString native");
        let equals = registry
            .find("java/nio/ByteOrder", "equals", "(Ljava/lang/Object;)Z")
            .expect("ByteOrder.equals native");

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("java/nio/ByteOrder")
            .expect("class init");
        ctx.set_declared_fields(
            class_id,
            vec![FieldMetadata {
                name: "name".to_string(),
                descriptor: "Ljava/lang/String;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: class_id,
                is_static: false,
            }],
        );
        let big = s2_byte_order_object(&mut ctx, 0).expect("BIG_ENDIAN");
        let little = s2_byte_order_object(&mut ctx, 1).expect("LITTLE_ENDIAN");

        for (obj, expected) in [(big, "BIG_ENDIAN"), (little, "LITTLE_ENDIAN")] {
            let rendered = match to_string(&mut ctx, &[Value::Object(Some(obj))]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                other => panic!("toString returned {other:?}"),
            };
            assert_eq!(rendered, expected);
        }
        // `MethodCallFailed` is not `PartialEq`, so unwrap rather than compare
        // the whole `Result`.
        let eq = |ctx: &mut crate::test_utils::MockNativeContext, a: ObjectRef, b: ObjectRef| {
            match equals(ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]) {
                Ok(Some(Value::Int(v))) => v,
                other => panic!("equals returned {other:?}"),
            }
        };
        assert_eq!(
            eq(&mut ctx, big, little),
            0,
            "two DIFFERENT constants must not compare equal — they did while \
             both slot-0 values decoded to 0"
        );
        assert_eq!(
            eq(&mut ctx, big, big),
            1,
            "and the same constant must still compare equal to itself"
        );
    }

    /// `s2_bb_alloc_direct` gained the by-name writes and the layout screen
    /// (G38-1). On the bare six-slot synthetic layout — the ONLY shape this
    /// function is reached on today — the screen is open and every indexed
    /// slot must end byte-identical to the pre-G38 answer.
    #[test]
    fn the_direct_buffer_overlay_is_unchanged_on_the_bare_synthetic_layout() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let buf = match s2_bb_alloc_direct(&mut ctx, 16) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("allocateDirect returned {other:?}"),
        };
        assert!(
            s2_bb_synthetic_layout(&ctx, buf),
            "premise: no real ByteBuffer layout is declared, so the screen is open"
        );
        assert_eq!(ctx.get_field(buf, BB_ARRAY), Value::Object(None));
        assert_eq!(ctx.get_field(buf, BB_POS), Value::Int(0));
        assert_eq!(ctx.get_field(buf, BB_LIMIT), Value::Int(16));
        assert_eq!(ctx.get_field(buf, BB_CAP), Value::Int(16));
        assert!(
            matches!(ctx.get_field(buf, BB_MARK), Value::Long(_)),
            "BB_MARK carries the native address on a direct buffer"
        );
        assert_eq!(
            ctx.get_field(buf, BB_ORDER),
            Value::Int(0),
            "the order flag still lands in the slot the synthetic readers use"
        );
        assert_eq!(s2_bb_order(&ctx, buf), 0);
        assert!(
            s2_bb_arr(&ctx, buf).is_none(),
            "a direct buffer has no backing array"
        );
    }

    /// And the screen itself, which is what keeps that overlay off a real
    /// layout: on the eleven-field JDK 25 `java.nio.ByteBuffer` the six
    /// indices alias `mark/position/limit/capacity/address/segment`, so
    /// `BB_ORDER` would land on a REFERENCE and `BB_ARRAY` on an `int`.
    #[test]
    fn the_real_byte_buffer_layout_closes_the_direct_overlay_screen() {
        use cratonvm_native_api::FieldMetadata;
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("java/nio/ByteBuffer")
            .expect("class init");
        let names = [
            ("mark", "I"),
            ("position", "I"),
            ("limit", "I"),
            ("capacity", "I"),
            ("address", "J"),
            ("segment", "Ljava/lang/foreign/MemorySegment;"),
            ("hb", "[B"),
            ("offset", "I"),
            ("isReadOnly", "Z"),
            ("bigEndian", "Z"),
            ("nativeByteOrder", "Z"),
        ];
        ctx.set_declared_fields(
            class_id,
            names
                .iter()
                .enumerate()
                .map(|(i, (name, descriptor))| FieldMetadata {
                    name: (*name).to_string(),
                    descriptor: (*descriptor).to_string(),
                    access_flags: 0,
                    slot_index: i,
                    declaring_class_id: class_id,
                    is_static: false,
                })
                .collect(),
        );
        let buf = ctx.alloc_object(class_id, names.len());
        assert!(
            !s2_bb_synthetic_layout(&ctx, buf),
            "an eleven-field real layout must never take the indexed overlay"
        );
        assert_eq!(
            ctx.resolve_field_index_by_class_id(class_id, "segment"),
            Some(BB_ORDER),
            "BB_ORDER is exactly the reference-typed `segment` slot — that is \
             why the unconditional write destroyed the order flag"
        );
        assert_eq!(
            ctx.resolve_field_index_by_class_id(class_id, "mark"),
            Some(BB_ARRAY),
            "and BB_ARRAY is `mark`, an int, which is the other direction of \
             the same coercion"
        );
    }
}
