// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Xerces XML parser intrinsics (`com.sun.org.apache.xerces.*`) and the XML scanner fast paths.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

/// Per-VM shared Xerces grammar pool: `vm_identity -> global-root handle` in
/// that VM's global-ref table.
///
/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 3): the row used to carry a raw `ObjectRef` fallback
/// (`lib.rs::SpringXmlGrammarPoolEntry`), answered whenever the root was `0`
/// (no global-ref table) or no longer resolved -- an address from before any
/// number of moving collections. Only the handle is kept now: a pool that
/// cannot be rooted is used for its one call and not cached, and a handle
/// that no longer resolves is a miss (its row is replaced). The VM's row is
/// dropped at teardown by [`forget_vm_spring_xml_grammar_pool`]; the old
/// replace path also ran `remove_global_root` and the resolve while holding
/// this lock.
fn spring_xml_grammar_pool_store(
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, usize>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// VM teardown (`lib.rs::forget_vm_native_root_stores`): drop `vm`'s pool row,
/// releasing nothing (the VM's global-ref table dies with it). Idempotent.
pub fn forget_vm_spring_xml_grammar_pool(vm: usize) {
    spring_xml_grammar_pool_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm);
}

pub fn spring_xml_shared_grammar_pool(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let scope = ctx.vm_identity();
    let cached = spring_xml_grammar_pool_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&scope)
        .copied();
    if let Some(pool) = cached.and_then(|root| ctx.resolve_global_root(root)) {
        return Some(pool);
    }

    let pool = match ctx.new_object_initialized(
        "com/sun/org/apache/xerces/internal/util/XMLGrammarPoolImpl",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(pool)))) => pool,
        _ => return None,
    };
    let root = ctx.add_global_root(pool);
    if root == 0 {
        // No global-ref table (or it is full): use this pool for this call
        // and cache nothing -- a raw address must not outlive the call.
        return Some(pool);
    }

    // Publish, unless a racing (or re-entrant, via the constructor) call
    // published a pool that still resolves: then answer that one and drop
    // ours. Handles are copied out of the guard before any `ctx` call.
    let published = {
        let mut store = spring_xml_grammar_pool_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match store.get(&scope).copied() {
            Some(existing) if Some(existing) != cached => Err(existing),
            _ => {
                store.insert(scope, root);
                Ok(cached)
            }
        }
    };
    match published {
        Ok(replaced) => {
            // `replaced` no longer resolves (we got here on a miss); its
            // slot is released so the table does not keep a dead entry.
            if let Some(old) = replaced {
                let _ = ctx.remove_global_root(old);
            }
            Some(pool)
        }
        Err(existing) => match ctx.resolve_global_root(existing) {
            Some(winner) => {
                let _ = ctx.remove_global_root(root);
                Some(winner)
            }
            None => {
                // The winner's handle is dead too: keep ours.
                spring_xml_grammar_pool_store()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(scope, root);
                Some(pool)
            }
        },
    }
}

pub fn spring_xml_set_factory_bool(
    ctx: &mut dyn NativeContext,
    factory: ObjectRef,
    method: &str,
    value: bool,
) -> MethodCallResult {
    let value = if value { 1 } else { 0 };
    ctx.invoke_virtual(factory, method, "(Z)V", &[Value::Int(value)])?;
    Ok(None)
}

pub fn spring_xml_set_factory_attribute(
    ctx: &mut dyn NativeContext,
    factory: ObjectRef,
    name: &str,
    value: Value,
) -> MethodCallResult {
    // `create_string` may run a moving collection.  This helper is called
    // with both a long-lived DocumentBuilderFactory and (for schema mode) a
    // shared grammar-pool object, so neither raw reference may be used after
    // that allocation without first rooting and refreshing it.
    let factory_pin = ctx.pin_native_root(factory);
    let value_pin = match value {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let name = ctx.create_string(name);
    let factory = ctx.read_native_pin(factory_pin, factory);
    let value = match value_pin {
        Some((pin, obj)) => Value::Object(Some(ctx.read_native_pin(pin, obj))),
        None => value,
    };
    let result = ctx.invoke_virtual(
        factory,
        "setAttribute",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        &[Value::Object(Some(name)), value],
    );
    ctx.unpin_native_roots(factory_pin);
    result.map(|_| None)
}

fn native_xerces_cmstateset_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bit_count = cmstateset_int_field(ctx, this, "fBitCount");
    if bit_count < 65 {
        let bits1 = cmstateset_int_field(ctx, this, "fBits1");
        let bits2 = cmstateset_int_field(ctx, this, "fBits2");
        return Ok(Some(Value::Int(bits1.wrapping_add(bits2.wrapping_mul(31)))));
    }

    let byte_count = cmstateset_int_field(ctx, this, "fByteCount");
    let mut hash = 0i32;
    if let Some(bytes) = cmstateset_read_bytes(ctx, this, byte_count) {
        for byte in bytes.iter().rev() {
            hash = (*byte as i8 as i32).wrapping_add(hash.wrapping_mul(31));
        }
    }
    Ok(Some(Value::Int(hash)))
}

fn native_xerces_cmstateset_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }
    let other_class = ctx.class_name_of_id(ctx.class_id_of_object(other));
    if other_class.as_deref() != Some(XERCES_CMSTATESET) {
        return Ok(Some(Value::Int(0)));
    }
    Ok(Some(Value::Int(if cmstateset_same_set(ctx, this, other) {
        1
    } else {
        0
    })))
}

fn native_xerces_cmstateset_is_same_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    Ok(Some(Value::Int(if cmstateset_same_set(ctx, this, other) {
        1
    } else {
        0
    })))
}

pub fn register_xerces_cmstateset_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        XERCES_CMSTATESET,
        "hashCode",
        "()I",
        native_xerces_cmstateset_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_CMSTATESET,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_xerces_cmstateset_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_CMSTATESET,
        "isSameSet",
        "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z",
        native_xerces_cmstateset_is_same_set, cratonvm_native_api::NativeKind::Bridge,
    );
}

fn native_xerces_xmlchar_is_space(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(
        if (0..=0x20).contains(&c) && xmlchar_has_mask(ctx, c, XMLCHAR_MASK_SPACE) {
            1
        } else {
            0
        },
    )))
}

fn native_xerces_xmlchar_is_name_start(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(
        if xmlchar_has_mask(ctx, c, XMLCHAR_MASK_NAME_START) {
            1
        } else {
            0
        },
    )))
}

fn native_xerces_xmlchar_is_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(
        if xmlchar_has_mask(ctx, c, XMLCHAR_MASK_NAME) {
            1
        } else {
            0
        },
    )))
}

fn native_xerces_xmlchar_is_ncname_start(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(
        if xmlchar_has_mask(ctx, c, XMLCHAR_MASK_NCNAME_START) {
            1
        } else {
            0
        },
    )))
}

fn native_xerces_xmlchar_is_ncname(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(
        if xmlchar_has_mask(ctx, c, XMLCHAR_MASK_NCNAME) {
            1
        } else {
            0
        },
    )))
}

fn xml_limit_field_obj(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, field_name) {
        Value::Object(Some(obj)) => Ok(obj),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("XMLLimitAnalyzer.{field_name} is null")),
        }
        .into()),
    }
}

fn xml_limit_slot(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    index: i32,
) -> Result<usize, MethodCallFailed> {
    // Xerces' `array[index]`: the array access's AIOOBE text, "Index i out
    // of bounds for length n" (wave 100 L7; was "Array index out of range: i").
    let array_len = ctx.array_length(array);
    if index < 0 || index as usize >= array_len {
        let length = i32::try_from(array_len).unwrap_or(i32::MAX);
        return Err(RuntimeError::aioobe(index, length).into());
    }
    let slot = index as usize;
    Ok(slot)
}

fn xml_limit_int_at(ctx: &dyn NativeContext, array: ObjectRef, slot: usize) -> i32 {
    ctx.get_array_element(array, slot).as_int().unwrap_or(0)
}

fn xml_limit_set_int(ctx: &dyn NativeContext, array: ObjectRef, slot: usize, value: i32) {
    ctx.set_array_element(array, slot, Value::Int(value));
}

fn xml_limit_add_int(ctx: &dyn NativeContext, array: ObjectRef, slot: usize, delta: i32) {
    let value = xml_limit_int_at(ctx, array, slot).wrapping_add(delta);
    xml_limit_set_int(ctx, array, slot, value);
}

fn xml_limit_ref_at(ctx: &dyn NativeContext, array: ObjectRef, slot: usize) -> Option<ObjectRef> {
    match ctx.get_array_element(array, slot) {
        Value::Object(Some(obj)) => Some(obj),
        _ => None,
    }
}

fn xml_limit_value_array(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    xml_limit_field_obj(ctx, this, "values")
}

fn xml_limit_total_array(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    xml_limit_field_obj(ctx, this, "totalValue")
}

fn xml_limit_ordinal(
    ctx: &mut dyn NativeContext,
    limit: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    match ctx.invoke_virtual(limit, "ordinal", "()I", &[])? {
        Some(Value::Int(index)) => Ok(index),
        Some(Value::Long(index)) => Ok(index as i32),
        _ => Ok(0),
    }
}

fn xml_limit_value_arg(args: &[Value], index: usize) -> i32 {
    args.get(index).and_then(|v| v.as_int()).unwrap_or(0)
}

fn xml_limit_entity_arg(args: &[Value], index: usize) -> Option<ObjectRef> {
    match args.get(index) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

fn xml_limit_put_cached_value(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    index: i32,
    entity_name: Option<ObjectRef>,
    value: i32,
) -> MethodCallResult {
    let values = xml_limit_field_obj(ctx, this, "values")?;
    let total_value = xml_limit_field_obj(ctx, this, "totalValue")?;
    let names = xml_limit_field_obj(ctx, this, "names")?;
    let caches = xml_limit_field_obj(ctx, this, "caches")?;
    let slot = xml_limit_slot(ctx, caches, index)?;
    xml_limit_slot(ctx, values, index)?;
    xml_limit_slot(ctx, total_value, index)?;
    xml_limit_slot(ctx, names, index)?;

    let base_pin = ctx.pin_native_root(this);
    let values_pin = ctx.pin_native_root(values);
    let total_pin = ctx.pin_native_root(total_value);
    let names_pin = ctx.pin_native_root(names);
    let caches_pin = ctx.pin_native_root(caches);
    let entity_pin = entity_name.map(|obj| (ctx.pin_native_root(obj), obj));

    let result: Result<(), MethodCallFailed> = (|| {
        let mut caches_now = ctx.read_native_pin(caches_pin, caches);
        let mut cache = xml_limit_ref_at(ctx, caches_now, slot);
        let mut cache_pin = cache.map(|obj| (ctx.pin_native_root(obj), obj));
        if cache.is_none() {
            let map =
                match ctx.new_object_initialized("java/util/HashMap", "(I)V", &[Value::Int(10)])? {
                    Some(Value::Object(Some(map))) => map,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("HashMap allocation returned null".to_string()),
                        }
                        .into())
                    }
                };
            caches_now = ctx.read_native_pin(caches_pin, caches);
            ctx.set_array_element(caches_now, slot, Value::Object(Some(map)));
            cache = Some(map);
            cache_pin = Some((ctx.pin_native_root(map), map));
        }

        let (cache_pin_handle, cache_fallback) = cache_pin.expect("cache exists after allocation");
        let entity_value = match entity_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => Value::Object(None),
        };

        let cache_now = ctx.read_native_pin(cache_pin_handle, cache_fallback);
        let contains = match ctx.invoke_virtual(
            cache_now,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            &[entity_value],
        )? {
            Some(Value::Int(v)) => v != 0,
            _ => false,
        };

        let entity_value = match entity_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => Value::Object(None),
        };
        let mut accumulated_value = value;
        if contains {
            let cache_now = ctx.read_native_pin(cache_pin_handle, cache_fallback);
            if let Some(Value::Object(Some(old_box))) = ctx.invoke_virtual(
                cache_now,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[entity_value],
            )? {
                accumulated_value =
                    accumulated_value.wrapping_add(ctx.get_field(old_box, 0).as_int().unwrap_or(0));
            }
        }

        // Typed so the stale-handle audit sees the binding (gc-common w19-f).
        let boxed: ObjectRef = crate::lang_math::alloc_wrapper(ctx, "java/lang/Integer");
        ctx.set_field(boxed, 0, Value::Int(accumulated_value));
        let boxed_pin = ctx.pin_native_root(boxed);
        let entity_value = match entity_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => Value::Object(None),
        };
        let cache_now = ctx.read_native_pin(cache_pin_handle, cache_fallback);
        let boxed_now = ctx.read_native_pin(boxed_pin, boxed);
        ctx.invoke_virtual(
            cache_now,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[entity_value, Value::Object(Some(boxed_now))],
        )?;

        let values_now = ctx.read_native_pin(values_pin, values);
        if accumulated_value > xml_limit_int_at(ctx, values_now, slot) {
            xml_limit_set_int(ctx, values_now, slot, accumulated_value);
            let names_now = ctx.read_native_pin(names_pin, names);
            let name_value = match entity_pin {
                Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
                None => Value::Object(None),
            };
            ctx.set_array_element(names_now, slot, name_value);
        }

        if index == XML_LIMIT_GENERAL_ENTITY_SIZE || index == XML_LIMIT_PARAMETER_ENTITY_SIZE {
            let total_now = ctx.read_native_pin(total_pin, total_value);
            let total_slot = xml_limit_slot(ctx, total_now, XML_LIMIT_TOTAL_ENTITY_SIZE)?;
            xml_limit_add_int(ctx, total_now, total_slot, value);
        }
        Ok(())
    })();

    ctx.unpin_native_roots(base_pin);
    result?;
    Ok(None)
}

fn xml_limit_add_value_index(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    index: i32,
    entity_name: Option<ObjectRef>,
    value: i32,
) -> MethodCallResult {
    if matches!(
        index,
        XML_LIMIT_ENTITY_EXPANSION
            | XML_LIMIT_MAX_OCCUR_NODE
            | XML_LIMIT_ELEMENT_ATTRIBUTE
            | XML_LIMIT_TOTAL_ENTITY_SIZE
            | XML_LIMIT_ENTITY_REPLACEMENT
    ) {
        let total_value = xml_limit_total_array(ctx, this)?;
        let slot = xml_limit_slot(ctx, total_value, index)?;
        xml_limit_add_int(ctx, total_value, slot, value);
        return Ok(None);
    }

    if matches!(index, XML_LIMIT_MAX_ELEMENT_DEPTH | XML_LIMIT_MAX_NAME) {
        let values = xml_limit_value_array(ctx, this)?;
        let total_value = xml_limit_total_array(ctx, this)?;
        let value_slot = xml_limit_slot(ctx, values, index)?;
        let total_slot = xml_limit_slot(ctx, total_value, index)?;
        xml_limit_set_int(ctx, values, value_slot, value);
        xml_limit_set_int(ctx, total_value, total_slot, value);
        return Ok(None);
    }

    xml_limit_put_cached_value(ctx, this, index, entity_name, value)
}

fn native_xml_limit_analyzer_add_value_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = xml_limit_value_arg(args, 1);
    let entity_name = xml_limit_entity_arg(args, 2);
    let value = xml_limit_value_arg(args, 3);
    xml_limit_add_value_index(ctx, this, index, entity_name, value)
}

fn native_xml_limit_analyzer_add_value_limit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let limit = obj_arg(args, 1)?;
    let entity_name = xml_limit_entity_arg(args, 2);
    let value = xml_limit_value_arg(args, 3);
    let base_pin = ctx.pin_native_root(this);
    let limit_pin = ctx.pin_native_root(limit);
    let entity_pin = entity_name.map(|obj| (ctx.pin_native_root(obj), obj));
    let result: MethodCallResult = (|| {
        let limit_now = ctx.read_native_pin(limit_pin, limit);
        let index = xml_limit_ordinal(ctx, limit_now)?;
        let this_now = ctx.read_native_pin(base_pin, this);
        let entity_now = entity_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        xml_limit_add_value_index(ctx, this_now, index, entity_now, value)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn xml_limit_analyzer_get_value_index(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    index: i32,
) -> Result<i32, MethodCallFailed> {
    if index == XML_LIMIT_ENTITY_REPLACEMENT {
        let total_value = xml_limit_total_array(ctx, this)?;
        let slot = xml_limit_slot(ctx, total_value, index)?;
        return Ok(xml_limit_int_at(ctx, total_value, slot));
    }
    let values = xml_limit_value_array(ctx, this)?;
    let slot = xml_limit_slot(ctx, values, index)?;
    Ok(xml_limit_int_at(ctx, values, slot))
}

fn native_xml_limit_analyzer_get_value_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = xml_limit_value_arg(args, 1);
    Ok(Some(Value::Int(xml_limit_analyzer_get_value_index(
        ctx, this, index,
    )?)))
}

fn native_xml_limit_analyzer_get_value_limit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let limit = obj_arg(args, 1)?;
    let base_pin = ctx.pin_native_root(this);
    let limit_pin = ctx.pin_native_root(limit);
    let result: MethodCallResult = (|| {
        let limit_now = ctx.read_native_pin(limit_pin, limit);
        let index = xml_limit_ordinal(ctx, limit_now)?;
        let this_now = ctx.read_native_pin(base_pin, this);
        Ok(Some(Value::Int(xml_limit_analyzer_get_value_index(
            ctx, this_now, index,
        )?)))
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_xml_limit_analyzer_get_total_value_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = xml_limit_value_arg(args, 1);
    let total_value = xml_limit_total_array(ctx, this)?;
    let slot = xml_limit_slot(ctx, total_value, index)?;
    Ok(Some(Value::Int(xml_limit_int_at(ctx, total_value, slot))))
}

fn native_xml_limit_analyzer_get_total_value_limit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let limit = obj_arg(args, 1)?;
    let base_pin = ctx.pin_native_root(this);
    let limit_pin = ctx.pin_native_root(limit);
    let result: MethodCallResult = (|| {
        let limit_now = ctx.read_native_pin(limit_pin, limit);
        let index = xml_limit_ordinal(ctx, limit_now)?;
        let this_now = ctx.read_native_pin(base_pin, this);
        let total_value = xml_limit_total_array(ctx, this_now)?;
        let slot = xml_limit_slot(ctx, total_value, index)?;
        Ok(Some(Value::Int(xml_limit_int_at(ctx, total_value, slot))))
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_xml_limit_analyzer_get_value_by_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = xml_limit_value_arg(args, 1);
    let values = xml_limit_value_array(ctx, this)?;
    let slot = xml_limit_slot(ctx, values, index)?;
    Ok(Some(Value::Int(xml_limit_int_at(ctx, values, slot))))
}

fn xssimple_short_field(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> i32 {
    ctx.get_field_by_name(this, field_name)
        .as_int()
        .unwrap_or(0)
}

fn xssimple_dv_normalize_type(ctx: &mut dyn NativeContext, validation_dv: i32) -> i32 {
    if validation_dv < 0 {
        return XSSIMPLE_NORMALIZE_NONE;
    }

    if let Some(class_id) = ctx
        .class_id_by_name(XERCES_XSSIMPLE_TYPE_DECL)
        .or_else(|| ctx.ensure_class_initialized(XERCES_XSSIMPLE_TYPE_DECL).ok())
    {
        if let Some(field_index) = ctx.static_field_index_by_name(class_id, "fDVNormalizeType") {
            if let Value::Object(Some(array)) = ctx.get_static_field(class_id, field_index) {
                let slot = validation_dv as usize;
                if slot < ctx.array_length(array) {
                    return ctx.get_array_element(array, slot).as_int().unwrap_or(0);
                }
            }
        }
    }

    XSSIMPLE_DV_NORMALIZE_TYPE_FALLBACK
        .get(validation_dv as usize)
        .copied()
        .unwrap_or(XSSIMPLE_NORMALIZE_NONE)
}

fn xssimple_is_ws(unit: u16) -> bool {
    matches!(unit, 0x09 | 0x0a | 0x0d | 0x20)
}

fn xssimple_normalize_units(units: &[u16], whitespace: i32) -> Vec<u16> {
    if units.is_empty() || whitespace == 0 {
        return units.to_vec();
    }

    if whitespace == 1 {
        return units
            .iter()
            .map(|&unit| {
                if matches!(unit, 0x09 | 0x0a | 0x0d) {
                    0x20
                } else {
                    unit
                }
            })
            .collect();
    }

    let mut out = Vec::with_capacity(units.len());
    let mut leading = true;
    let mut index = 0usize;
    while index < units.len() {
        let unit = units[index];
        if !xssimple_is_ws(unit) {
            out.push(unit);
            leading = false;
            index += 1;
            continue;
        }

        while index + 1 < units.len() && xssimple_is_ws(units[index + 1]) {
            index += 1;
        }
        if index < units.len() - 1 && !leading {
            out.push(0x20);
        }
        index += 1;
    }
    out
}

fn xssimple_xmlchar_trim_units(units: &[u16]) -> Vec<u16> {
    let mut start = 0usize;
    let mut end = units.len();
    while start < end && xssimple_is_ws(units[start]) {
        start += 1;
    }
    while end > start && xssimple_is_ws(units[end - 1]) {
        end -= 1;
    }
    units[start..end].to_vec()
}

fn xssimple_units_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

fn xssimple_string_units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn xssimple_create_string(ctx: &mut dyn NativeContext, units: &[u16]) -> ObjectRef {
    let text = xssimple_units_to_string(units);
    ctx.create_string_uninterned(&text)
}

fn xssimple_object_is_string_buffer(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(obj);
    if ctx.class_name_arc_of_id(class_id).as_deref() == Some("java/lang/StringBuffer") {
        return true;
    }
    ctx.class_id_by_name("java/lang/StringBuffer")
        .is_some_and(|string_buffer_id| ctx.is_subclass(class_id, string_buffer_id))
}

fn xssimple_to_string_ref(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<Option<(ObjectRef, String)>, MethodCallFailed> {
    if let Some(text) = ctx.read_string(obj) {
        return Ok(Some((obj, text)));
    }

    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[])? {
        Some(Value::Object(Some(str_ref))) => {
            let text = ctx
                .read_string(str_ref)
                .unwrap_or_else(|| "null".to_string());
            Ok(Some((str_ref, text)))
        }
        Some(Value::Object(None)) => Ok(None),
        _ => {
            let text = crate::lang_string::invoke_to_string(ctx, obj)?;
            let str_ref = ctx.create_string_uninterned(&text);
            Ok(Some((str_ref, text)))
        }
    }
}

fn native_xssimple_type_normalize_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(content)) = args.first() else {
        return Ok(Some(Value::Object(None)));
    };
    let whitespace = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let Some(content) = *content else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(text) = ctx.read_string(content) else {
        return Ok(Some(Value::Object(Some(content))));
    };
    if text.is_empty() || whitespace == 0 {
        return Ok(Some(Value::Object(Some(content))));
    }

    let normalized = xssimple_normalize_units(&xssimple_string_units(&text), whitespace);
    Ok(Some(Value::Object(Some(xssimple_create_string(
        ctx,
        &normalized,
    )))))
}

fn native_xssimple_type_normalize_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(Value::Object(value)) = args.get(1) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(value) = *value else {
        return Ok(Some(Value::Object(None)));
    };
    let whitespace = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);

    if (xssimple_short_field(ctx, this, "fFacetsDefined") & XSSIMPLE_FACET_PATTERN) == 0 {
        let validation_dv = xssimple_short_field(ctx, this, "fValidationDV");
        match xssimple_dv_normalize_type(ctx, validation_dv) {
            XSSIMPLE_NORMALIZE_NONE => {
                return Ok(xssimple_to_string_ref(ctx, value)?
                    .map(|(str_ref, _)| Value::Object(Some(str_ref)))
                    .or(Some(Value::Object(None))));
            }
            XSSIMPLE_NORMALIZE_TRIM => {
                let Some((str_ref, text)) = xssimple_to_string_ref(ctx, value)? else {
                    return Ok(Some(Value::Object(None)));
                };
                let units = xssimple_string_units(&text);
                let trimmed = xssimple_xmlchar_trim_units(&units);
                if trimmed.len() == units.len() {
                    return Ok(Some(Value::Object(Some(str_ref))));
                }
                return Ok(Some(Value::Object(Some(xssimple_create_string(
                    ctx, &trimmed,
                )))));
            }
            _ => {}
        }
    }

    if xssimple_object_is_string_buffer(ctx, value) {
        let units = crate::lang_string::sb_read_chars(ctx, value);
        if units.is_empty() {
            return Ok(Some(Value::Object(Some(ctx.create_string("")))));
        }
        if whitespace == 0 {
            return Ok(Some(Value::Object(Some(xssimple_create_string(
                ctx, &units,
            )))));
        }
        let normalized = xssimple_normalize_units(&units, whitespace);
        crate::lang_string::sb_write_chars(ctx, value, &normalized);
        return Ok(Some(Value::Object(Some(xssimple_create_string(
            ctx,
            &normalized,
        )))));
    }

    let Some((str_ref, text)) = xssimple_to_string_ref(ctx, value)? else {
        return Ok(Some(Value::Object(None)));
    };
    if text.is_empty() || whitespace == 0 {
        return Ok(Some(Value::Object(Some(str_ref))));
    }
    let normalized = xssimple_normalize_units(&xssimple_string_units(&text), whitespace);
    Ok(Some(Value::Object(Some(xssimple_create_string(
        ctx,
        &normalized,
    )))))
}

fn xml_entity_scanner_name_type_ordinal(
    ctx: &mut dyn NativeContext,
    nt: Option<ObjectRef>,
) -> Result<Option<i32>, MethodCallFailed> {
    let Some(nt) = nt else {
        return Ok(None);
    };
    match ctx.invoke_virtual(nt, "ordinal", "()I", &[])? {
        Some(Value::Int(value)) => Ok(Some(value)),
        Some(Value::Long(value)) => Ok(Some(value as i32)),
        _ => Ok(Some(0)),
    }
}

fn xml_entity_scanner_check_entity_limit_impl(
    ctx: &mut dyn NativeContext,
    scanner: ObjectRef,
    nt: Option<ObjectRef>,
    entity: Option<ObjectRef>,
    _offset: i32,
    length: i32,
) -> MethodCallResult {
    let Some(entity) = entity else {
        return Ok(None);
    };
    if !bool_field_named(ctx, entity, "isGE") {
        return Ok(None);
    }

    let scanner_pin = ctx.pin_native_root(scanner);
    let entity_pin = ctx.pin_native_root(entity);
    let nt_pin = nt.map(|obj| (ctx.pin_native_root(obj), obj));
    let result = (|| -> MethodCallResult {
        let nt_now = nt_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let nt_ordinal = xml_entity_scanner_name_type_ordinal(ctx, nt_now)?;
        let scanner_now = ctx.read_native_pin(scanner_pin, scanner);
        let entity_now = ctx.read_native_pin(entity_pin, entity);
        let analyzer = object_field_ref(ctx, scanner_now, "fLimitAnalyzer");
        let entity_name = object_field_ref(ctx, entity_now, "name");

        if let Some(analyzer) = analyzer {
            if nt_ordinal != Some(9) {
                xml_limit_add_value_index(
                    ctx,
                    analyzer,
                    XML_LIMIT_GENERAL_ENTITY_SIZE,
                    entity_name,
                    length,
                )?;
            }
            if matches!(nt_ordinal, Some(1 | 4)) {
                xml_limit_add_value_index(
                    ctx,
                    analyzer,
                    XML_LIMIT_ENTITY_REPLACEMENT,
                    entity_name,
                    1,
                )?;
            }
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(scanner_pin);
    result
}

fn native_xml_entity_scanner_check_entity_limit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let scanner = obj_arg(args, 0)?;
    let nt = match args.get(1) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };
    let entity = match args.get(2) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };
    let offset = args.get(3).and_then(|value| value.as_int()).unwrap_or(0);
    let length = args.get(4).and_then(|value| value.as_int()).unwrap_or(0);
    xml_entity_scanner_check_entity_limit_impl(ctx, scanner, nt, entity, offset, length)
}

fn xml_entity_scanner_check_name_limit_impl(
    ctx: &mut dyn NativeContext,
    scanner: ObjectRef,
    entity: ObjectRef,
    length: i32,
) -> MethodCallResult {
    if length <= 0 {
        return Ok(None);
    }
    let Some(analyzer) = object_field_ref(ctx, scanner, "fLimitAnalyzer") else {
        return Ok(None);
    };
    let entity_name = object_field_ref(ctx, entity, "name");
    xml_limit_add_value_index(ctx, analyzer, XML_LIMIT_MAX_NAME, entity_name, length)
}

fn xml_entity_scanner_units(
    ctx: &dyn NativeContext,
    chars: ObjectRef,
    offset: i32,
    length: i32,
) -> Result<Vec<u16>, MethodCallFailed> {
    if offset < 0 || length < 0 {
        return Err(RuntimeError::aioobe_index_only(offset).into());
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| RuntimeError::aioobe_index_only(offset.wrapping_add(length)))?;
    if end as usize > ctx.array_length(chars) {
        return Err(RuntimeError::aioobe_index_only(end).into());
    }
    let mut units = Vec::with_capacity(length as usize);
    for slot in offset..end {
        units.push(xml_entity_scanner_char_at(ctx, chars, slot)? as u16);
    }
    Ok(units)
}

fn xml_entity_scanner_symbol(
    ctx: &mut dyn NativeContext,
    scanner: ObjectRef,
    chars: ObjectRef,
    offset: i32,
    length: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(symbol_table) = object_field_ref(ctx, scanner, "fSymbolTable") {
        if let Some(Value::Object(Some(symbol))) = ctx.invoke_virtual(
            symbol_table,
            "addSymbol",
            "([CII)Ljava/lang/String;",
            &[
                Value::Object(Some(chars)),
                Value::Int(offset),
                Value::Int(length),
            ],
        )? {
            return Ok(symbol);
        }
    }

    let units = xml_entity_scanner_units(ctx, chars, offset, length)?;
    Ok(ctx.create_string_uninterned(&String::from_utf16_lossy(&units)))
}

fn xml_entity_scanner_set_qname_values(
    ctx: &dyn NativeContext,
    qname: ObjectRef,
    prefix: Option<ObjectRef>,
    localpart: Option<ObjectRef>,
    rawname: Option<ObjectRef>,
    uri: Option<ObjectRef>,
) {
    ctx.set_field_by_name(qname, "prefix", Value::Object(prefix));
    ctx.set_field_by_name(qname, "localpart", Value::Object(localpart));
    ctx.set_field_by_name(qname, "rawname", Value::Object(rawname));
    ctx.set_field_by_name(qname, "uri", Value::Object(uri));
}

fn xerces_valid_ascii_name_unit(unit: i32) -> bool {
    (b'A' as i32..=b'Z' as i32).contains(&unit)
        || (b'a' as i32..=b'z' as i32).contains(&unit)
        || (b'0' as i32..=b'9' as i32).contains(&unit)
        || unit == b'-' as i32
        || unit == b'.' as i32
        || unit == b':' as i32
        || unit == b'_' as i32
}

fn xerces_is_name_start_unit(
    ctx: &dyn NativeContext,
    chars_table: Option<ObjectRef>,
    unit: i32,
) -> bool {
    if let Some(table) = chars_table {
        xmlchar_has_mask_in_table(ctx, table, unit, XMLCHAR_MASK_NAME_START)
    } else {
        (b'A' as i32..=b'Z' as i32).contains(&unit)
            || (b'a' as i32..=b'z' as i32).contains(&unit)
            || unit == b'_' as i32
            || unit == b':' as i32
    }
}

fn xerces_is_name_unit(ctx: &dyn NativeContext, chars_table: Option<ObjectRef>, unit: i32) -> bool {
    if unit < 127 && xerces_valid_ascii_name_unit(unit) {
        return true;
    }
    chars_table.is_some_and(|table| xmlchar_has_mask_in_table(ctx, table, unit, XMLCHAR_MASK_NAME))
}

fn xerces_is_ncname_start_unit(
    ctx: &dyn NativeContext,
    chars_table: Option<ObjectRef>,
    unit: i32,
) -> bool {
    if let Some(table) = chars_table {
        xmlchar_has_mask_in_table(ctx, table, unit, XMLCHAR_MASK_NCNAME_START)
    } else {
        (b'A' as i32..=b'Z' as i32).contains(&unit)
            || (b'a' as i32..=b'z' as i32).contains(&unit)
            || unit == b'_' as i32
    }
}

fn native_xml_entity_scanner_scan_qname(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let scanner = obj_arg(args, 0)?;
    let qname = obj_arg(args, 1)?;
    let nt = match args.get(2) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };
    let scanner_pin = ctx.pin_native_root(scanner);
    let qname_pin = ctx.pin_native_root(qname);
    let nt_pin = nt.map(|obj| (ctx.pin_native_root(obj), obj));
    let result = (|| -> MethodCallResult {
        let mut scanner = ctx.read_native_pin(scanner_pin, scanner);
        let mut qname = ctx.read_native_pin(qname_pin, qname);
        let mut entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;

        if int_field_named(ctx, entity, "position") == int_field_named(ctx, entity, "count") {
            ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "load",
                "(IZZ)Z",
                &[
                    Value::Object(Some(scanner)),
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(1),
                ],
            )?;
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            qname = ctx.read_native_pin(qname_pin, qname);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
        }

        let offset = int_field_named(ctx, entity, "position");
        set_int_field_named(ctx, scanner, "offset", offset);
        let mut chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        let chars_pin = ctx.pin_native_root(chars);
        // The `XMLChar.CHARS` table is a heap array like every other local
        // here, and this scan loop calls back into Java (`load`,
        // `invokeListeners`, `addSymbol`) — each of which can allocate and so
        // move it. Every other object in this function is already held through
        // a pin and re-read after such a call; the table was the one raw
        // `ObjectRef` left, and a stale one reads garbage masks, which is
        // indistinguishable from "this character is not a name character".
        // `unpin_native_roots(scanner_pin)` at the end covers this pin too.
        let chars_table_pin = xmlchar_chars_array(ctx).map(|t| (ctx.pin_native_root(t), t));
        scanner = ctx.read_native_pin(scanner_pin, scanner);
        entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        chars = ctx.read_native_pin(chars_pin, chars);
        let first = xml_entity_scanner_char_at(ctx, chars, offset)?;
        let chars_table = chars_table_pin.map(|(pin, t)| ctx.read_native_pin(pin, t));
        if !xerces_is_name_start_unit(ctx, chars_table, first) {
            return Ok(Some(Value::Int(0)));
        }

        let mut position = offset.wrapping_add(1);
        set_int_field_named(ctx, entity, "position", position);
        if position == int_field_named(ctx, entity, "count") {
            ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "invokeListeners",
                "(I)V",
                &[Value::Object(Some(scanner)), Value::Int(1)],
            )?;
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            xml_entity_scanner_set_char(ctx, chars, 0, first)?;
            set_int_field_named(ctx, scanner, "offset", 0);
            let loaded = ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "load",
                "(IZZ)Z",
                &[
                    Value::Object(Some(scanner)),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(0),
                ],
            )?;
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            qname = ctx.read_native_pin(qname_pin, qname);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            if loaded.and_then(|value| value.as_int()).unwrap_or(0) != 0 {
                set_int_field_named(
                    ctx,
                    entity,
                    "columnNumber",
                    int_field_named(ctx, entity, "columnNumber").wrapping_add(1),
                );
                chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                    RuntimeError::NullPointerException {
                        message: Some("ScannedEntity.ch is null".to_string()),
                    }
                })?;
                let name = xml_entity_scanner_symbol(ctx, scanner, chars, 0, 1)?;
                qname = ctx.read_native_pin(qname_pin, qname);
                xml_entity_scanner_set_qname_values(ctx, qname, None, Some(name), Some(name), None);
                let nt_now = nt_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
                xml_entity_scanner_check_entity_limit_impl(
                    ctx,
                    scanner,
                    nt_now,
                    Some(entity),
                    0,
                    1,
                )?;
                return Ok(Some(Value::Int(1)));
            }
            set_int_field_named(ctx, entity, "position", 1);
            position = 1;
        }

        let mut index = -1;
        loop {
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            position = int_field_named(ctx, entity, "position");
            if position >= int_field_named(ctx, entity, "count") {
                break;
            }
            let c = xml_entity_scanner_char_at(ctx, chars, position)?;
            let chars_table = chars_table_pin.map(|(pin, t)| ctx.read_native_pin(pin, t));
            if !xerces_is_name_unit(ctx, chars_table, c) {
                break;
            }
            if c == ':' as i32 {
                if index != -1 {
                    break;
                }
                index = position;
                let offset_now = int_field_named(ctx, scanner, "offset");
                xml_entity_scanner_check_name_limit_impl(
                    ctx,
                    scanner,
                    entity,
                    index.wrapping_sub(offset_now),
                )?;
                scanner = ctx.read_native_pin(scanner_pin, scanner);
                entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                    RuntimeError::NullPointerException {
                        message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                    }
                })?;
            }

            let offset_now = int_field_named(ctx, scanner, "offset");
            let next_position = int_field_named(ctx, entity, "position").wrapping_add(1);
            set_int_field_named(ctx, entity, "position", next_position);
            if next_position == int_field_named(ctx, entity, "count") {
                let length = next_position.wrapping_sub(offset_now);
                let name_length = if index != -1 {
                    length.wrapping_sub(index.wrapping_sub(offset_now))
                } else {
                    length
                };
                xml_entity_scanner_check_name_limit_impl(ctx, scanner, entity, name_length)?;
                ctx.invoke(
                    XERCES_XML_ENTITY_SCANNER,
                    "invokeListeners",
                    "(I)V",
                    &[Value::Object(Some(scanner)), Value::Int(length)],
                )?;
                scanner = ctx.read_native_pin(scanner_pin, scanner);
                entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                    RuntimeError::NullPointerException {
                        message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                    }
                })?;
                chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                    RuntimeError::NullPointerException {
                        message: Some("ScannedEntity.ch is null".to_string()),
                    }
                })?;
                if length as usize == ctx.array_length(chars) {
                    let buffer_size = int_field_named(ctx, entity, "fBufferSize");
                    let new_len = if buffer_size > 0 {
                        (buffer_size as usize).saturating_mul(2)
                    } else {
                        ctx.array_length(chars)
                            .saturating_mul(2)
                            .max(length as usize)
                    };
                    let tmp = ctx.new_array(cratonvm_types::ArrayElementType::Char, new_len);
                    // gc-common w19-f: the allocation can collect. The copy used
                    // to read the old `ch` and write `ch`, `fBufferSize` and the
                    // scanner's `offset` through pre-allocation addresses;
                    // re-derive all three from the scanner's pin.
                    scanner = ctx.read_native_pin(scanner_pin, scanner);
                    entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                        }
                    })?;
                    chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("ScannedEntity.ch is null".to_string()),
                        }
                    })?;
                    for slot in 0..length {
                        let value = xml_entity_scanner_char_at(ctx, chars, offset_now + slot)?;
                        xml_entity_scanner_set_char(ctx, tmp, slot, value)?;
                    }
                    ctx.set_field_by_name(entity, "ch", Value::Object(Some(tmp)));
                    if buffer_size > 0 {
                        set_int_field_named(
                            ctx,
                            entity,
                            "fBufferSize",
                            buffer_size.saturating_mul(2),
                        );
                    }
                } else {
                    for slot in 0..length {
                        let value = xml_entity_scanner_char_at(ctx, chars, offset_now + slot)?;
                        xml_entity_scanner_set_char(ctx, chars, slot, value)?;
                    }
                }
                if index != -1 {
                    index = index.wrapping_sub(offset_now);
                }
                set_int_field_named(ctx, scanner, "offset", 0);
                let loaded = ctx.invoke(
                    XERCES_XML_ENTITY_SCANNER,
                    "load",
                    "(IZZ)Z",
                    &[
                        Value::Object(Some(scanner)),
                        Value::Int(length),
                        Value::Int(0),
                        Value::Int(0),
                    ],
                )?;
                if loaded.and_then(|value| value.as_int()).unwrap_or(0) != 0 {
                    break;
                }
            }
        }

        scanner = ctx.read_native_pin(scanner_pin, scanner);
        qname = ctx.read_native_pin(qname_pin, qname);
        entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        let offset = int_field_named(ctx, scanner, "offset");
        let length = int_field_named(ctx, entity, "position").wrapping_sub(offset);
        set_int_field_named(
            ctx,
            entity,
            "columnNumber",
            int_field_named(ctx, entity, "columnNumber").wrapping_add(length),
        );
        if length <= 0 {
            return Ok(Some(Value::Int(0)));
        }

        let rawname = xml_entity_scanner_symbol(ctx, scanner, chars, offset, length)?;
        let rawname_pin = ctx.pin_native_root(rawname);
        scanner = ctx.read_native_pin(scanner_pin, scanner);
        qname = ctx.read_native_pin(qname_pin, qname);
        entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;

        let (prefix_pin, localpart_pin) = if index != -1 {
            let prefix_length = index.wrapping_sub(offset);
            xml_entity_scanner_check_name_limit_impl(ctx, scanner, entity, prefix_length)?;
            let prefix = xml_entity_scanner_symbol(ctx, scanner, chars, offset, prefix_length)?;
            let prefix_pin = ctx.pin_native_root(prefix);
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            chars = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            let local_length = length.wrapping_sub(prefix_length).wrapping_sub(1);
            let local_start = index.wrapping_add(1);
            let local_first = xml_entity_scanner_char_at(ctx, chars, local_start)?;
            let chars_table = chars_table_pin.map(|(pin, t)| ctx.read_native_pin(pin, t));
            if !xerces_is_ncname_start_unit(ctx, chars_table, local_first) {
                // The JDK reports IllegalQName here and still finishes the scan.
                // The cold error-reporting path is left to interpreted Xerces.
            }
            xml_entity_scanner_check_name_limit_impl(ctx, scanner, entity, local_length)?;
            let localpart =
                xml_entity_scanner_symbol(ctx, scanner, chars, local_start, local_length)?;
            let localpart_pin = ctx.pin_native_root(localpart);
            (Some((prefix_pin, prefix)), Some((localpart_pin, localpart)))
        } else {
            xml_entity_scanner_check_name_limit_impl(ctx, scanner, entity, length)?;
            (None, Some((rawname_pin, rawname)))
        };

        qname = ctx.read_native_pin(qname_pin, qname);
        let prefix = prefix_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let localpart = localpart_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let rawname = ctx.read_native_pin(rawname_pin, rawname);
        xml_entity_scanner_set_qname_values(ctx, qname, prefix, localpart, Some(rawname), None);
        scanner = ctx.read_native_pin(scanner_pin, scanner);
        entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        let nt_now = nt_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        xml_entity_scanner_check_entity_limit_impl(
            ctx,
            scanner,
            nt_now,
            Some(entity),
            offset,
            length,
        )?;
        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(scanner_pin);
    result
}

fn xerces_normalized_newline(version: i32, unit: i32, is_external: bool, initial: bool) -> bool {
    unit == '\n' as i32
        || unit == '\r' as i32
        || (version == 2 && (unit == 0x85 || unit == 0x2028) && (!initial || is_external))
}

fn xml_entity_scanner_char_at(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    index: i32,
) -> Result<i32, MethodCallFailed> {
    // Xerces' `array[index]`: the array access's AIOOBE text (wave 100 L7).
    let array_len = ctx.array_length(array);
    if index < 0 || index as usize >= array_len {
        let length = i32::try_from(array_len).unwrap_or(i32::MAX);
        return Err(RuntimeError::aioobe(index, length).into());
    }
    Ok(ctx
        .get_array_element(array, index as usize)
        .as_int()
        .unwrap_or(0))
}

fn xml_entity_scanner_set_char(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    index: i32,
    value: i32,
) -> Result<(), MethodCallFailed> {
    // Xerces' `array[index]`: the array access's AIOOBE text (wave 100 L7).
    let array_len = ctx.array_length(array);
    if index < 0 || index as usize >= array_len {
        let length = i32::try_from(array_len).unwrap_or(i32::MAX);
        return Err(RuntimeError::aioobe(index, length).into());
    }
    ctx.set_array_element(array, index as usize, Value::Int(value));
    Ok(())
}

fn xml_string_set_values(
    ctx: &dyn NativeContext,
    xml_string: ObjectRef,
    ch: ObjectRef,
    offset: i32,
    length: i32,
) {
    ctx.set_field_by_name(xml_string, "ch", Value::Object(Some(ch)));
    set_int_field_named(ctx, xml_string, "offset", offset);
    set_int_field_named(ctx, xml_string, "length", length);
}

fn xml_entity_scanner_normalize_newlines_impl(
    ctx: &mut dyn NativeContext,
    scanner: ObjectRef,
    version: i32,
    xml_string: ObjectRef,
    append: bool,
    store_ws: bool,
    nt: Option<ObjectRef>,
) -> MethodCallResult {
    let scanner_pin = ctx.pin_native_root(scanner);
    let xml_string_pin = ctx.pin_native_root(xml_string);
    let nt_pin = nt.map(|obj| (ctx.pin_native_root(obj), obj));
    let result = (|| -> MethodCallResult {
        let mut scanner = ctx.read_native_pin(scanner_pin, scanner);
        let mut xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
        let mut entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        let mut offset = int_field_named(ctx, entity, "position");
        set_int_field_named(ctx, scanner, "offset", offset);
        set_int_field_named(ctx, scanner, "newlines", 0);
        set_int_field_named(ctx, scanner, "counted", 0);

        let mut ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        let first = xml_entity_scanner_char_at(ctx, ch, offset)?;
        let is_external = bool_field_named(ctx, scanner, "isExternal");
        if !xerces_normalized_newline(version, first, is_external, true) {
            return Ok(Some(Value::Int(0)));
        }

        loop {
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            let position = int_field_named(ctx, entity, "position");
            let c = xml_entity_scanner_char_at(ctx, ch, position)?;
            set_int_field_named(ctx, entity, "position", position.wrapping_add(1));

            if xerces_normalized_newline(version, c, is_external, false) {
                let newlines = int_field_named(ctx, scanner, "newlines").wrapping_add(1);
                set_int_field_named(ctx, scanner, "newlines", newlines);
                set_int_field_named(
                    ctx,
                    entity,
                    "lineNumber",
                    int_field_named(ctx, entity, "lineNumber").wrapping_add(1),
                );
                set_int_field_named(ctx, entity, "columnNumber", 1);

                if int_field_named(ctx, entity, "position") == int_field_named(ctx, entity, "count")
                {
                    let nt_now = nt_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
                    xml_entity_scanner_check_entity_limit_impl(
                        ctx,
                        scanner,
                        nt_now,
                        Some(entity),
                        offset,
                        newlines,
                    )?;
                    scanner = ctx.read_native_pin(scanner_pin, scanner);
                    entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                        }
                    })?;
                    offset = 0;
                    set_int_field_named(ctx, scanner, "offset", 0);
                    set_int_field_named(ctx, entity, "position", newlines);
                    ctx.invoke(
                        XERCES_XML_ENTITY_SCANNER,
                        "load",
                        "(IZZ)Z",
                        &[
                            Value::Object(Some(scanner)),
                            Value::Int(newlines),
                            Value::Int(0),
                            Value::Int(1),
                        ],
                    )?;
                    scanner = ctx.read_native_pin(scanner_pin, scanner);
                    entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                        }
                    })?;
                    if int_field_named(ctx, entity, "position") != newlines {
                        set_int_field_named(ctx, scanner, "counted", 1);
                        break;
                    }
                }

                if c == '\r' as i32 && is_external {
                    ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("ScannedEntity.ch is null".to_string()),
                        }
                    })?;
                    let position = int_field_named(ctx, entity, "position");
                    let cc = xml_entity_scanner_char_at(ctx, ch, position)?;
                    if cc == '\n' as i32 || (version == 2 && cc == 0x85) {
                        set_int_field_named(ctx, entity, "position", position.wrapping_add(1));
                        offset = offset.wrapping_add(1);
                        set_int_field_named(ctx, scanner, "offset", offset);
                    } else {
                        set_int_field_named(
                            ctx,
                            scanner,
                            "newlines",
                            int_field_named(ctx, scanner, "newlines").wrapping_add(1),
                        );
                    }
                }
            } else {
                set_int_field_named(
                    ctx,
                    entity,
                    "position",
                    int_field_named(ctx, entity, "position").wrapping_sub(1),
                );
                break;
            }

            if int_field_named(ctx, entity, "position")
                >= int_field_named(ctx, entity, "count").wrapping_sub(1)
            {
                break;
            }
        }

        scanner = ctx.read_native_pin(scanner_pin, scanner);
        xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
        entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        offset = int_field_named(ctx, scanner, "offset");
        let position = int_field_named(ctx, entity, "position");
        for slot in offset..position {
            xml_entity_scanner_set_char(ctx, ch, slot, '\n' as i32)?;
            if store_ws {
                ctx.invoke(
                    XERCES_XML_ENTITY_SCANNER,
                    "storeWhiteSpace",
                    "(I)V",
                    &[Value::Object(Some(scanner)), Value::Int(slot)],
                )?;
            }
        }

        scanner = ctx.read_native_pin(scanner_pin, scanner);
        xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
        entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        let length = int_field_named(ctx, entity, "position").wrapping_sub(offset);
        if int_field_named(ctx, entity, "position")
            == int_field_named(ctx, entity, "count").wrapping_sub(1)
        {
            let nt_now = nt_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
            xml_entity_scanner_check_entity_limit_impl(
                ctx,
                scanner,
                nt_now,
                Some(entity),
                offset,
                length,
            )?;
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            if append {
                ctx.invoke_virtual(
                    xml_string,
                    "append",
                    "([CII)V",
                    &[
                        Value::Object(Some(ch)),
                        Value::Int(offset),
                        Value::Int(length),
                    ],
                )?;
            } else {
                xml_string_set_values(ctx, xml_string, ch, offset, length);
            }
            return Ok(Some(Value::Int(1)));
        }

        Ok(Some(Value::Int(0)))
    })();
    ctx.unpin_native_roots(scanner_pin);
    result
}

fn native_xml_entity_scanner_normalize_newlines(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let scanner = obj_arg(args, 0)?;
    let version = args.get(1).and_then(|value| value.as_int()).unwrap_or(1);
    let xml_string = obj_arg(args, 2)?;
    let append = args.get(3).and_then(|value| value.as_int()).unwrap_or(0) != 0;
    let store_ws = args.get(4).and_then(|value| value.as_int()).unwrap_or(0) != 0;
    let nt = match args.get(5) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };
    xml_entity_scanner_normalize_newlines_impl(
        ctx, scanner, version, xml_string, append, store_ws, nt,
    )
}

fn native_xml_entity_scanner_scan_content(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let xml_string = obj_arg(args, 1)?;
    let root_base = ctx.pin_native_root(this);
    let xml_string_pin = ctx.pin_native_root(xml_string);

    let result = (|| -> MethodCallResult {
        let mut this = ctx.read_native_pin(root_base, this);
        let mut xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
        let mut entity = object_field_ref(ctx, this, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        let mut position = int_field_named(ctx, entity, "position");
        let count = int_field_named(ctx, entity, "count");

        if position == count {
            ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "load",
                "(IZZ)Z",
                &[
                    Value::Object(Some(this)),
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(1),
                ],
            )?;
            this = ctx.read_native_pin(root_base, this);
            xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
        } else if position == count - 1 {
            ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "invokeListeners",
                "(I)V",
                &[Value::Object(Some(this)), Value::Int(1)],
            )?;
            this = ctx.read_native_pin(root_base, this);
            xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
            entity = object_field_ref(ctx, this, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            let ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            let last = ctx.get_array_element(ch, (count - 1).max(0) as usize);
            ctx.set_array_element(ch, 0, last);
            ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "load",
                "(IZZ)Z",
                &[
                    Value::Object(Some(this)),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(0),
                ],
            )?;
            this = ctx.read_native_pin(root_base, this);
            xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
            entity = object_field_ref(ctx, this, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            set_int_field_named(ctx, entity, "position", 0);        }

        let normalized = xml_entity_scanner_normalize_newlines_impl(
            ctx, this, 1, xml_string, false, false, None,
        )?;
        if normalized.and_then(|v| v.as_int()).unwrap_or(0) != 0 {
            return Ok(Some(Value::Int(-1)));
        }

        // Same stale-local rule as `scanQName` above: this loop invokes Java
        // (`load`, `invokeListeners`) while holding the table.
        let chars_table_pin = xmlchar_chars_array(ctx).map(|t| (ctx.pin_native_root(t), t));
        this = ctx.read_native_pin(root_base, this);
        xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
        entity = object_field_ref(ctx, this, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        let ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        position = int_field_named(ctx, entity, "position");
        let count = int_field_named(ctx, entity, "count");
        while position < count {
            let slot = position as usize;
            let unit = ctx.get_array_element(ch, slot).as_int().unwrap_or(0);
            position = position.wrapping_add(1);
            let chars_table = chars_table_pin.map(|(pin, t)| ctx.read_native_pin(pin, t));
            let is_content = if let Some(table) = chars_table {
                xmlchar_has_mask_in_table(ctx, table, unit, XMLCHAR_MASK_CONTENT)
            } else {
                unit != '<' as i32 && unit != '&' as i32 && unit != '\r' as i32
            };
            if !is_content {
                position = position.wrapping_sub(1);
                break;
            }
        }
        set_int_field_named(ctx, entity, "position", position);

        let offset = int_field_named(ctx, this, "offset");
        let length = position.wrapping_sub(offset);
        let column = int_field_named(ctx, entity, "columnNumber")
            .wrapping_add(length)
            .wrapping_sub(int_field_named(ctx, this, "newlines"));
        set_int_field_named(ctx, entity, "columnNumber", column);

        if !bool_field_named(ctx, this, "counted") {
            xml_entity_scanner_check_entity_limit_impl(
                ctx,
                this,
                None,
                Some(entity),
                offset,
                length,
            )?;
            this = ctx.read_native_pin(root_base, this);
            xml_string = ctx.read_native_pin(xml_string_pin, xml_string);
            entity = object_field_ref(ctx, this, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
        }

        let ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        ctx.set_field_by_name(xml_string, "ch", Value::Object(Some(ch)));
        set_int_field_named(ctx, xml_string, "offset", offset);
        set_int_field_named(ctx, xml_string, "length", length);

        let count = int_field_named(ctx, entity, "count");
        let mut next = if position != count {
            ctx.get_array_element(ch, position as usize)
                .as_int()
                .unwrap_or(0)
        } else {
            -1
        };
        if next == 13 && bool_field_named(ctx, this, "isExternal") {
            next = 10;
        }
        Ok(Some(Value::Int(next)))
    })();

    ctx.unpin_native_roots(root_base);
    result
}

fn xerces_xml_is_space_unit(unit: i32) -> bool {
    matches!(unit, 0x20 | 0x09 | 0x0a | 0x0d)
}

fn native_xml_entity_scanner_skip_spaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let scanner = obj_arg(args, 0)?;
    let scanner_pin = ctx.pin_native_root(scanner);
    let result = (|| -> MethodCallResult {
        let mut scanner = ctx.read_native_pin(scanner_pin, scanner);
        let mut entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
            }
        })?;
        if int_field_named(ctx, entity, "position") == int_field_named(ctx, entity, "count") {
            ctx.invoke(
                XERCES_XML_ENTITY_SCANNER,
                "load",
                "(IZZ)Z",
                &[
                    Value::Object(Some(scanner)),
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(1),
                ],
            )?;
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            let Some(current) = object_field_ref(ctx, scanner, "fCurrentEntity") else {
                return Ok(Some(Value::Int(0)));
            };
            entity = current;
        }

        let mut ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("ScannedEntity.ch is null".to_string()),
            }
        })?;
        let mut c = xml_entity_scanner_char_at(ctx, ch, int_field_named(ctx, entity, "position"))?;
        let mut offset = int_field_named(ctx, entity, "position").wrapping_sub(1);
        set_int_field_named(ctx, scanner, "offset", offset);
        if !xerces_xml_is_space_unit(c) {
            return Ok(Some(Value::Int(0)));
        }

        loop {
            let mut entity_changed = false;
            let is_external = bool_field_named(ctx, scanner, "isExternal");
            if c == '\n' as i32 || (is_external && c == '\r' as i32) {
                set_int_field_named(
                    ctx,
                    entity,
                    "lineNumber",
                    int_field_named(ctx, entity, "lineNumber").wrapping_add(1),
                );
                set_int_field_named(ctx, entity, "columnNumber", 1);
                if int_field_named(ctx, entity, "position")
                    == int_field_named(ctx, entity, "count").wrapping_sub(1)
                {
                    ctx.invoke(
                        XERCES_XML_ENTITY_SCANNER,
                        "invokeListeners",
                        "(I)V",
                        &[Value::Object(Some(scanner)), Value::Int(1)],
                    )?;
                    scanner = ctx.read_native_pin(scanner_pin, scanner);
                    entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                        }
                    })?;
                    ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("ScannedEntity.ch is null".to_string()),
                        }
                    })?;
                    xml_entity_scanner_set_char(ctx, ch, 0, c)?;
                    let loaded = ctx.invoke(
                        XERCES_XML_ENTITY_SCANNER,
                        "load",
                        "(IZZ)Z",
                        &[
                            Value::Object(Some(scanner)),
                            Value::Int(1),
                            Value::Int(1),
                            Value::Int(0),
                        ],
                    )?;
                    entity_changed = loaded.and_then(|value| value.as_int()).unwrap_or(0) != 0;
                    scanner = ctx.read_native_pin(scanner_pin, scanner);
                    if !entity_changed {
                        entity =
                            object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                                RuntimeError::NullPointerException {
                                    message: Some(
                                        "XMLEntityScanner.fCurrentEntity is null".to_string(),
                                    ),
                                }
                            })?;
                        set_int_field_named(ctx, entity, "position", 0);
                    } else if object_field_ref(ctx, scanner, "fCurrentEntity").is_none() {
                        return Ok(Some(Value::Int(1)));
                    }
                }

                scanner = ctx.read_native_pin(scanner_pin, scanner);
                entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                    RuntimeError::NullPointerException {
                        message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                    }
                })?;
                if c == '\r' as i32 && is_external {
                    ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                        RuntimeError::NullPointerException {
                            message: Some("ScannedEntity.ch is null".to_string()),
                        }
                    })?;
                    let next_position = int_field_named(ctx, entity, "position").wrapping_add(1);
                    set_int_field_named(ctx, entity, "position", next_position);
                    if xml_entity_scanner_char_at(ctx, ch, next_position)? != '\n' as i32 {
                        set_int_field_named(ctx, entity, "position", next_position.wrapping_sub(1));
                    }
                }
            } else {
                set_int_field_named(
                    ctx,
                    entity,
                    "columnNumber",
                    int_field_named(ctx, entity, "columnNumber").wrapping_add(1),
                );
            }

            let skipped_length = int_field_named(ctx, entity, "position").wrapping_sub(offset);
            xml_entity_scanner_check_entity_limit_impl(
                ctx,
                scanner,
                None,
                Some(entity),
                offset,
                skipped_length,
            )?;
            scanner = ctx.read_native_pin(scanner_pin, scanner);
            entity = object_field_ref(ctx, scanner, "fCurrentEntity").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("XMLEntityScanner.fCurrentEntity is null".to_string()),
                }
            })?;
            offset = int_field_named(ctx, entity, "position");
            set_int_field_named(ctx, scanner, "offset", offset);

            if !entity_changed {
                set_int_field_named(
                    ctx,
                    entity,
                    "position",
                    int_field_named(ctx, entity, "position").wrapping_add(1),
                );
            }

            if int_field_named(ctx, entity, "position") == int_field_named(ctx, entity, "count") {
                ctx.invoke(
                    XERCES_XML_ENTITY_SCANNER,
                    "load",
                    "(IZZ)Z",
                    &[
                        Value::Object(Some(scanner)),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(1),
                    ],
                )?;
                scanner = ctx.read_native_pin(scanner_pin, scanner);
                let Some(current) = object_field_ref(ctx, scanner, "fCurrentEntity") else {
                    return Ok(Some(Value::Int(1)));
                };
                entity = current;
            }

            ch = object_field_ref(ctx, entity, "ch").ok_or_else(|| {
                RuntimeError::NullPointerException {
                    message: Some("ScannedEntity.ch is null".to_string()),
                }
            })?;
            c = xml_entity_scanner_char_at(ctx, ch, int_field_named(ctx, entity, "position"))?;
            if !xerces_xml_is_space_unit(c) {
                break;
            }
        }

        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(scanner_pin);
    result
}

fn xerces_opti_string_field(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> Value {
    match ctx.get_field_by_name(this, field_name) {
        Value::Object(obj) => Value::Object(obj),
        _ => Value::Object(None),
    }
}

fn native_xerces_opti_node_get_node_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(xerces_opti_string_field(ctx, this, "rawname")))
}

fn native_xerces_opti_node_get_namespace_uri(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(xerces_opti_string_field(ctx, this, "uri")))
}

fn native_xerces_opti_node_get_prefix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(xerces_opti_string_field(ctx, this, "prefix")))
}

fn native_xerces_opti_node_get_local_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(xerces_opti_string_field(ctx, this, "localpart")))
}

fn native_xerces_opti_node_get_node_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(int_field_named(ctx, this, "nodeType"))))
}

fn native_xerces_opti_node_get_read_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(if bool_field_named(ctx, this, "hidden") {
        1
    } else {
        0
    })))
}

fn native_xerces_opti_element_get_tag_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_xerces_opti_node_get_node_name(ctx, args)
}

fn native_xerces_opti_attr_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_xerces_opti_node_get_node_name(ctx, args)
}

fn native_xerces_opti_attr_get_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(xerces_opti_string_field(ctx, this, "value")))
}

fn native_xerces_opti_attr_get_node_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_xerces_opti_attr_get_value(ctx, args)
}

fn native_xerces_opti_attr_get_specified(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn native_xerces_opti_attr_is_id(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_xerces_regex_range_token_sort_ranges(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if bool_field_named(ctx, this, "sorted") {
        return Ok(None);
    }
    let ranges = match ctx.get_field_by_name(this, "ranges") {
        Value::Object(Some(ranges)) => ranges,
        _ => return Ok(None),
    };
    let len = ctx.array_length(ranges);
    if len <= 2 {
        set_int_field_named(ctx, this, "sorted", 1);
        return Ok(None);
    }

    let mut pairs = Vec::with_capacity(len / 2);
    let mut slot = 0;
    while slot + 1 < len {
        let start = ctx.get_array_element(ranges, slot).as_int().unwrap_or(0);
        let end = ctx
            .get_array_element(ranges, slot + 1)
            .as_int()
            .unwrap_or(0);
        pairs.push((start, end));
        slot += 2;
    }
    pairs.sort_by_key(|&(start, end)| (start, end));

    for (index, (start, end)) in pairs.into_iter().enumerate() {
        let slot = index * 2;
        ctx.set_array_element(ranges, slot, Value::Int(start));
        ctx.set_array_element(ranges, slot + 1, Value::Int(end));
    }
    set_int_field_named(ctx, this, "sorted", 1);
    Ok(None)
}

pub fn register_xerces_xml_parser_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        XERCES_XMLCHAR,
        "isSpace",
        "(I)Z",
        native_xerces_xmlchar_is_space, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isNameStart",
        "(I)Z",
        native_xerces_xmlchar_is_name_start, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isName",
        "(I)Z",
        native_xerces_xmlchar_is_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isNCNameStart",
        "(I)Z",
        native_xerces_xmlchar_is_ncname_start, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isNCName",
        "(I)Z",
        native_xerces_xmlchar_is_ncname, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "addValue",
        "(ILjava/lang/String;I)V",
        native_xml_limit_analyzer_add_value_index, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "addValue",
        &format!("(L{XML_SECURITY_LIMIT};Ljava/lang/String;I)V"),
        native_xml_limit_analyzer_add_value_limit, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getValue",
        "(I)I",
        native_xml_limit_analyzer_get_value_index, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getValue",
        &format!("(L{XML_SECURITY_LIMIT};)I"),
        native_xml_limit_analyzer_get_value_limit, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getTotalValue",
        "(I)I",
        native_xml_limit_analyzer_get_total_value_index, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getTotalValue",
        &format!("(L{XML_SECURITY_LIMIT};)I"),
        native_xml_limit_analyzer_get_total_value_limit, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getValueByIndex",
        "(I)I",
        native_xml_limit_analyzer_get_value_by_index, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XSSIMPLE_TYPE_DECL,
        "normalize",
        "(Ljava/lang/String;S)Ljava/lang/String;",
        native_xssimple_type_normalize_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XSSIMPLE_TYPE_DECL,
        "normalize",
        "(Ljava/lang/Object;S)Ljava/lang/String;",
        native_xssimple_type_normalize_object, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(XERCES_XSD_KEY, "hashCode", "()I", native_xsd_key_hash_code, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        XERCES_XSD_KEY,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_xsd_key_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "scanContent",
        &format!("(L{XERCES_XML_STRING};)I"),
        native_xml_entity_scanner_scan_content, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "scanQName",
        &format!("(L{XERCES_QNAME};L{XERCES_XML_SCANNER_NAME_TYPE};)Z"),
        native_xml_entity_scanner_scan_qname, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "skipSpaces",
        "()Z",
        native_xml_entity_scanner_skip_spaces, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "normalizeNewlines",
        &format!("(SL{XERCES_XML_STRING};ZZL{XERCES_XML_SCANNER_NAME_TYPE};)Z"),
        native_xml_entity_scanner_normalize_newlines, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "checkEntityLimit",
        &format!("(L{XERCES_XML_SCANNER_NAME_TYPE};L{STREAM_SCANNED_ENTITY};II)V"),
        native_xml_entity_scanner_check_entity_limit, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getNodeName",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_node_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getNamespaceURI",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_namespace_uri, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getPrefix",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_prefix, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getLocalName",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_local_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getNodeType",
        "()S",
        native_xerces_opti_node_get_node_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getReadOnly",
        "()Z",
        native_xerces_opti_node_get_read_only, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_ELEMENT_IMPL,
        "getTagName",
        "()Ljava/lang/String;",
        native_xerces_opti_element_get_tag_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getName",
        "()Ljava/lang/String;",
        native_xerces_opti_attr_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getValue",
        "()Ljava/lang/String;",
        native_xerces_opti_attr_get_value, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getNodeValue",
        "()Ljava/lang/String;",
        native_xerces_opti_attr_get_node_value, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getSpecified",
        "()Z",
        native_xerces_opti_attr_get_specified, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "isId",
        "()Z",
        native_xerces_opti_attr_is_id, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        XERCES_REGEX_RANGE_TOKEN,
        "sortRanges",
        "()V",
        native_xerces_regex_range_token_sort_ranges, cratonvm_native_api::NativeKind::Bridge,
    );
}

#[cfg(test)]
mod xerces_cmstateset_tests;

#[cfg(test)]
mod xerces_xml_parser_tests;

#[cfg(test)]
mod w22a_grammar_pool_tests;
