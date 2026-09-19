// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Xerces XML parser intrinsics (`com.sun.org.apache.xerces.*`) and the XML scanner fast paths.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

fn spring_xml_grammar_pool_store(
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, SpringXmlGrammarPoolEntry>> {
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, SpringXmlGrammarPoolEntry>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn spring_xml_resolve_grammar_pool(
    ctx: &dyn NativeContext,
    entry: SpringXmlGrammarPoolEntry,
) -> Option<ObjectRef> {
    if entry.root != 0 {
        ctx.resolve_global_root(entry.root).or(Some(entry.fallback))
    } else {
        Some(entry.fallback)
    }
}

pub(crate) fn spring_xml_shared_grammar_pool(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let scope = ctx.vm_identity();
    if let Some(entry) = spring_xml_grammar_pool_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&scope)
        .copied()
    {
        if let Some(pool) = spring_xml_resolve_grammar_pool(ctx, entry) {
            return Some(pool);
        }
    }

    let pool = match ctx.new_object_initialized(
        "com/sun/org/apache/xerces/internal/util/XMLGrammarPoolImpl",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(pool)))) => pool,
        _ => return None,
    };
    let entry = SpringXmlGrammarPoolEntry {
        root: ctx.add_global_root(pool),
        fallback: pool,
    };

    let mut store = spring_xml_grammar_pool_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = store.get(&scope).copied() {
        if entry.root != 0 {
            let _ = ctx.remove_global_root(entry.root);
        }
        return spring_xml_resolve_grammar_pool(ctx, existing);
    }
    store.insert(scope, entry);
    Some(pool)
}

pub(crate) fn spring_xml_set_factory_bool(
    ctx: &mut dyn NativeContext,
    factory: ObjectRef,
    method: &str,
    value: bool,
) -> MethodCallResult {
    let value = if value { 1 } else { 0 };
    ctx.invoke_virtual(factory, method, "(Z)V", &[Value::Int(value)])?;
    Ok(None)
}

pub(crate) fn spring_xml_set_factory_attribute(
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

pub(crate) fn register_xerces_cmstateset_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        XERCES_CMSTATESET,
        "hashCode",
        "()I",
        native_xerces_cmstateset_hash_code,
    );
    registry.register(
        XERCES_CMSTATESET,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_xerces_cmstateset_equals,
    );
    registry.register(
        XERCES_CMSTATESET,
        "isSameSet",
        "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z",
        native_xerces_cmstateset_is_same_set,
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
    if index < 0 {
        return Err(RuntimeError::aioobe_index_only(index).into());
    }
    let slot = index as usize;
    if slot >= ctx.array_length(array) {
        return Err(RuntimeError::aioobe_index_only(index).into());
    }
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

        let boxed = crate::lang_math::alloc_wrapper(ctx, "java/lang/Integer");
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
    if index < 0 || index as usize >= ctx.array_length(array) {
        return Err(RuntimeError::aioobe_index_only(index).into());
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
    if index < 0 || index as usize >= ctx.array_length(array) {
        return Err(RuntimeError::aioobe_index_only(index).into());
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
            set_int_field_named(ctx, entity, "position", 0);
        }

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

pub(crate) fn register_xerces_xml_parser_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        XERCES_XMLCHAR,
        "isSpace",
        "(I)Z",
        native_xerces_xmlchar_is_space,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isNameStart",
        "(I)Z",
        native_xerces_xmlchar_is_name_start,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isName",
        "(I)Z",
        native_xerces_xmlchar_is_name,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isNCNameStart",
        "(I)Z",
        native_xerces_xmlchar_is_ncname_start,
    );
    registry.register(
        XERCES_XMLCHAR,
        "isNCName",
        "(I)Z",
        native_xerces_xmlchar_is_ncname,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "addValue",
        "(ILjava/lang/String;I)V",
        native_xml_limit_analyzer_add_value_index,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "addValue",
        &format!("(L{XML_SECURITY_LIMIT};Ljava/lang/String;I)V"),
        native_xml_limit_analyzer_add_value_limit,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getValue",
        "(I)I",
        native_xml_limit_analyzer_get_value_index,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getValue",
        &format!("(L{XML_SECURITY_LIMIT};)I"),
        native_xml_limit_analyzer_get_value_limit,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getTotalValue",
        "(I)I",
        native_xml_limit_analyzer_get_total_value_index,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getTotalValue",
        &format!("(L{XML_SECURITY_LIMIT};)I"),
        native_xml_limit_analyzer_get_total_value_limit,
    );
    registry.register(
        XML_LIMIT_ANALYZER,
        "getValueByIndex",
        "(I)I",
        native_xml_limit_analyzer_get_value_by_index,
    );
    registry.register(
        XERCES_XSSIMPLE_TYPE_DECL,
        "normalize",
        "(Ljava/lang/String;S)Ljava/lang/String;",
        native_xssimple_type_normalize_string,
    );
    registry.register(
        XERCES_XSSIMPLE_TYPE_DECL,
        "normalize",
        "(Ljava/lang/Object;S)Ljava/lang/String;",
        native_xssimple_type_normalize_object,
    );
    registry.register(XERCES_XSD_KEY, "hashCode", "()I", native_xsd_key_hash_code);
    registry.register(
        XERCES_XSD_KEY,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_xsd_key_equals,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "scanContent",
        &format!("(L{XERCES_XML_STRING};)I"),
        native_xml_entity_scanner_scan_content,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "scanQName",
        &format!("(L{XERCES_QNAME};L{XERCES_XML_SCANNER_NAME_TYPE};)Z"),
        native_xml_entity_scanner_scan_qname,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "skipSpaces",
        "()Z",
        native_xml_entity_scanner_skip_spaces,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "normalizeNewlines",
        &format!("(SL{XERCES_XML_STRING};ZZL{XERCES_XML_SCANNER_NAME_TYPE};)Z"),
        native_xml_entity_scanner_normalize_newlines,
    );
    registry.register(
        XERCES_XML_ENTITY_SCANNER,
        "checkEntityLimit",
        &format!("(L{XERCES_XML_SCANNER_NAME_TYPE};L{STREAM_SCANNED_ENTITY};II)V"),
        native_xml_entity_scanner_check_entity_limit,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getNodeName",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_node_name,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getNamespaceURI",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_namespace_uri,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getPrefix",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_prefix,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getLocalName",
        "()Ljava/lang/String;",
        native_xerces_opti_node_get_local_name,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getNodeType",
        "()S",
        native_xerces_opti_node_get_node_type,
    );
    registry.register(
        XERCES_OPTI_NODE_IMPL,
        "getReadOnly",
        "()Z",
        native_xerces_opti_node_get_read_only,
    );
    registry.register(
        XERCES_OPTI_ELEMENT_IMPL,
        "getTagName",
        "()Ljava/lang/String;",
        native_xerces_opti_element_get_tag_name,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getName",
        "()Ljava/lang/String;",
        native_xerces_opti_attr_get_name,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getValue",
        "()Ljava/lang/String;",
        native_xerces_opti_attr_get_value,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getNodeValue",
        "()Ljava/lang/String;",
        native_xerces_opti_attr_get_node_value,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "getSpecified",
        "()Z",
        native_xerces_opti_attr_get_specified,
    );
    registry.register(
        XERCES_OPTI_ATTR_IMPL,
        "isId",
        "()Z",
        native_xerces_opti_attr_is_id,
    );
    registry.register(
        XERCES_REGEX_RANGE_TOKEN,
        "sortRanges",
        "()V",
        native_xerces_regex_range_token_sort_ranges,
    );
}

#[cfg(test)]
mod xerces_cmstateset_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ArrayElementType;

    fn new_cmstateset(
        ctx: &mut MockNativeContext,
        bit_count: i32,
        byte_count: i32,
        bits1: i32,
        bits2: i32,
        bytes: &[u8],
    ) -> ObjectRef {
        let class_id = ctx
            .ensure_class_initialized(XERCES_CMSTATESET)
            .expect("CMStateSet class id");
        let obj = ctx.alloc_object(class_id, 5);
        ctx.set_field_by_name(obj, "fBitCount", Value::Int(bit_count));
        ctx.set_field_by_name(obj, "fByteCount", Value::Int(byte_count));
        ctx.set_field_by_name(obj, "fBits1", Value::Int(bits1));
        ctx.set_field_by_name(obj, "fBits2", Value::Int(bits2));
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (idx, byte) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, idx, Value::Int((*byte as i8) as i32));
        }
        ctx.set_field_by_name(obj, "fByteArray", Value::Object(Some(arr)));
        obj
    }

    fn java_cmstateset_byte_hash(bytes: &[u8]) -> i32 {
        let mut hash = 0i32;
        for byte in bytes.iter().rev() {
            hash = (*byte as i8 as i32).wrapping_add(hash.wrapping_mul(31));
        }
        hash
    }

    #[test]
    fn essential_natives_include_xerces_cmstateset_intrinsics() {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        for (name, descriptor) in [
            ("hashCode", "()I"),
            ("equals", "(Ljava/lang/Object;)Z"),
            (
                "isSameSet",
                "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z",
            ),
        ] {
            assert!(
                registry.find(XERCES_CMSTATESET, name, descriptor).is_some(),
                "CMStateSet.{name}{descriptor} must be in the real-JDK essential registry"
            );
        }
    }

    #[test]
    fn xerces_cmstateset_hash_code_matches_jdk_small_and_byte_array_paths() {
        let mut ctx = MockNativeContext::new();
        let small = new_cmstateset(&mut ctx, 64, 0, 17, -9, &[]);
        let small_result =
            native_xerces_cmstateset_hash_code(&mut ctx, &[Value::Object(Some(small))])
                .expect("small hash result")
                .expect("small hash value");
        assert_eq!(
            small_result,
            Value::Int(17i32.wrapping_add((-9i32).wrapping_mul(31)))
        );

        let bytes = [0x7f, 0x80, 0xff, 0x01];
        let large = new_cmstateset(&mut ctx, 65, bytes.len() as i32, 0, 0, &bytes);
        let large_result =
            native_xerces_cmstateset_hash_code(&mut ctx, &[Value::Object(Some(large))])
                .expect("large hash result")
                .expect("large hash value");
        assert_eq!(large_result, Value::Int(java_cmstateset_byte_hash(&bytes)));
    }

    #[test]
    fn xerces_cmstateset_equals_and_is_same_set_compare_real_fields() {
        let mut ctx = MockNativeContext::new();
        let a = new_cmstateset(&mut ctx, 65, 3, 0, 0, &[0xaa, 0x00, 0x7f]);
        let b = new_cmstateset(&mut ctx, 65, 3, 0, 0, &[0xaa, 0x00, 0x7f]);
        let c = new_cmstateset(&mut ctx, 65, 3, 0, 0, &[0xaa, 0x01, 0x7f]);

        let eq_ab = native_xerces_cmstateset_equals(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        )
        .expect("equals result")
        .expect("equals value");
        assert_eq!(eq_ab, Value::Int(1));

        let same_ac = native_xerces_cmstateset_is_same_set(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(c))],
        )
        .expect("isSameSet result")
        .expect("isSameSet value");
        assert_eq!(same_ac, Value::Int(0));
    }
}

#[cfg(test)]
mod xerces_xml_parser_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::FieldMetadata;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ArrayElementType;

    const XML_LIMIT_ARRAY_LEN: usize = 16;

    fn seed_xmlchar_table(ctx: &mut MockNativeContext) {
        let class_id = ctx
            .ensure_class_initialized(XERCES_XMLCHAR)
            .expect("XMLChar class id");
        ctx.set_declared_fields(
            class_id,
            vec![FieldMetadata {
                name: "CHARS".to_string(),
                descriptor: "[B".to_string(),
                access_flags: 0x0008,
                slot_index: 0,
                declaring_class_id: class_id,
                is_static: true,
            }],
        );
        let chars = ctx.new_array(ArrayElementType::Byte, 128);
        for ch in b'A'..=b'Z' {
            ctx.set_array_element(
                chars,
                ch as usize,
                Value::Int(
                    (XMLCHAR_MASK_NAME_START
                        | XMLCHAR_MASK_NAME
                        | XMLCHAR_MASK_NCNAME_START
                        | XMLCHAR_MASK_NCNAME) as i32,
                ),
            );
        }
        for ch in b'a'..=b'z' {
            ctx.set_array_element(
                chars,
                ch as usize,
                Value::Int(
                    (XMLCHAR_MASK_NAME_START
                        | XMLCHAR_MASK_NAME
                        | XMLCHAR_MASK_NCNAME_START
                        | XMLCHAR_MASK_NCNAME) as i32,
                ),
            );
        }
        for ch in b'0'..=b'9' {
            ctx.set_array_element(
                chars,
                ch as usize,
                Value::Int((XMLCHAR_MASK_NAME | XMLCHAR_MASK_NCNAME) as i32),
            );
        }
        for (ch, mask) in [
            (
                'A',
                XMLCHAR_MASK_NAME_START
                    | XMLCHAR_MASK_NAME
                    | XMLCHAR_MASK_NCNAME_START
                    | XMLCHAR_MASK_NCNAME,
            ),
            (
                '_',
                XMLCHAR_MASK_NAME_START
                    | XMLCHAR_MASK_NAME
                    | XMLCHAR_MASK_NCNAME_START
                    | XMLCHAR_MASK_NCNAME,
            ),
            (':', XMLCHAR_MASK_NAME_START | XMLCHAR_MASK_NAME),
            ('-', XMLCHAR_MASK_NAME | XMLCHAR_MASK_NCNAME),
            ('.', XMLCHAR_MASK_NAME | XMLCHAR_MASK_NCNAME),
            (' ', XMLCHAR_MASK_SPACE),
        ] {
            ctx.set_array_element(chars, ch as usize, Value::Int((mask as u8 as i8) as i32));
        }
        ctx.set_static_field(class_id, 0, Value::Object(Some(chars)));
    }

    fn new_xml_limit_analyzer(
        ctx: &mut MockNativeContext,
    ) -> (ObjectRef, ObjectRef, ObjectRef, ObjectRef, ObjectRef) {
        let class_id = ctx
            .ensure_class_initialized(XML_LIMIT_ANALYZER)
            .expect("XMLLimitAnalyzer class id");
        let analyzer = ctx.alloc_object(class_id, 6);
        let values = ctx.new_array(ArrayElementType::Int, XML_LIMIT_ARRAY_LEN);
        let names = ctx.new_array(ArrayElementType::Reference, XML_LIMIT_ARRAY_LEN);
        let total_value = ctx.new_array(ArrayElementType::Int, XML_LIMIT_ARRAY_LEN);
        let caches = ctx.new_array(ArrayElementType::Reference, XML_LIMIT_ARRAY_LEN);
        for slot in 0..XML_LIMIT_ARRAY_LEN {
            ctx.set_array_element(names, slot, Value::Object(None));
            ctx.set_array_element(caches, slot, Value::Object(None));
        }
        ctx.set_field_by_name(analyzer, "values", Value::Object(Some(values)));
        ctx.set_field_by_name(analyzer, "names", Value::Object(Some(names)));
        ctx.set_field_by_name(analyzer, "totalValue", Value::Object(Some(total_value)));
        ctx.set_field_by_name(analyzer, "caches", Value::Object(Some(caches)));
        (analyzer, values, names, total_value, caches)
    }

    fn new_xssimple_type_decl(
        ctx: &mut MockNativeContext,
        validation_dv: i32,
        facets_defined: i32,
    ) -> ObjectRef {
        let class_id = ctx
            .ensure_class_initialized(XERCES_XSSIMPLE_TYPE_DECL)
            .expect("XSSimpleTypeDecl class id");
        let decl = ctx.alloc_object(class_id, 14);
        ctx.set_field_by_name(decl, "fValidationDV", Value::Int(validation_dv));
        ctx.set_field_by_name(decl, "fFacetsDefined", Value::Int(facets_defined));
        decl
    }

    fn new_string_buffer(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
        let class_id = ctx
            .ensure_class_initialized("java/lang/StringBuffer")
            .expect("StringBuffer class id");
        let sb = ctx.alloc_object(class_id, 3);
        let units: Vec<u16> = text.encode_utf16().collect();
        let chars = ctx.new_array(ArrayElementType::Char, units.len());
        for (slot, unit) in units.iter().copied().enumerate() {
            ctx.set_array_element(chars, slot, Value::Int(unit as i32));
        }
        ctx.set_field(sb, 0, Value::Object(Some(chars)));
        ctx.set_field(sb, 1, Value::Int(0));
        ctx.set_field(sb, 2, Value::Int(units.len() as i32));
        sb
    }

    fn new_xsd_key(
        ctx: &mut MockNativeContext,
        system_id: Option<&str>,
        refer_ns: Option<ObjectRef>,
    ) -> ObjectRef {
        let class_id = ctx
            .ensure_class_initialized(XERCES_XSD_KEY)
            .expect("XSDKey class id");
        let key = ctx.alloc_object(class_id, 3);
        let system_id = system_id.map(|text| ctx.create_string(text));
        ctx.set_field_by_name(key, "systemId", Value::Object(system_id));
        ctx.set_field_by_name(key, "referType", Value::Int(0));
        ctx.set_field_by_name(key, "referNS", Value::Object(refer_ns));
        key
    }

    fn seed_xmlchar_content_table(ctx: &mut MockNativeContext) {
        seed_xmlchar_table(ctx);
        let class_id = ctx
            .class_id_by_name(XERCES_XMLCHAR)
            .expect("XMLChar class id");
        let chars = match ctx.get_static_field(class_id, 0) {
            Value::Object(Some(chars)) => chars,
            other => panic!("unexpected CHARS value: {other:?}"),
        };
        for ch in ['a', 'b', 'c', 't', 'i', 'l', ' '] {
            let old = ctx
                .get_array_element(chars, ch as usize)
                .as_int()
                .unwrap_or(0);
            ctx.set_array_element(chars, ch as usize, Value::Int(old | XMLCHAR_MASK_CONTENT));
        }
    }

    fn new_entity_scanner_fixture(
        ctx: &mut MockNativeContext,
        text: &str,
    ) -> (ObjectRef, ObjectRef, ObjectRef, ObjectRef) {
        let scanner_class = ctx
            .ensure_class_initialized(XERCES_XML_ENTITY_SCANNER)
            .expect("XMLEntityScanner class id");
        let entity_class = ctx
            .ensure_class_initialized(STREAM_SCANNED_ENTITY)
            .expect("ScannedEntity class id");
        let xml_string_class = ctx
            .ensure_class_initialized(XERCES_XML_STRING)
            .expect("XMLString class id");
        let scanner = ctx.alloc_object(scanner_class, 7);
        let entity = ctx.alloc_object(entity_class, 8);
        let xml_string = ctx.alloc_object(xml_string_class, 3);
        let units: Vec<u16> = text.encode_utf16().collect();
        let chars = ctx.new_array(ArrayElementType::Char, units.len());
        for (slot, unit) in units.iter().copied().enumerate() {
            ctx.set_array_element(chars, slot, Value::Int(unit as i32));
        }

        ctx.set_field_by_name(scanner, "fCurrentEntity", Value::Object(Some(entity)));
        ctx.set_field_by_name(scanner, "isExternal", Value::Int(0));
        ctx.set_field_by_name(scanner, "offset", Value::Int(0));
        ctx.set_field_by_name(scanner, "newlines", Value::Int(0));
        ctx.set_field_by_name(scanner, "counted", Value::Int(1));
        ctx.set_field_by_name(scanner, "fLimitAnalyzer", Value::Object(None));
        ctx.set_field_by_name(scanner, "fSymbolTable", Value::Object(None));
        ctx.set_field_by_name(entity, "ch", Value::Object(Some(chars)));
        ctx.set_field_by_name(entity, "position", Value::Int(0));
        ctx.set_field_by_name(entity, "count", Value::Int(units.len() as i32));
        ctx.set_field_by_name(entity, "columnNumber", Value::Int(0));
        ctx.set_field_by_name(entity, "lineNumber", Value::Int(1));
        ctx.set_field_by_name(entity, "isGE", Value::Int(0));
        ctx.set_field_by_name(entity, "name", Value::Object(None));
        ctx.set_field_by_name(entity, "fBufferSize", Value::Int(units.len() as i32));
        ctx.set_field_by_name(xml_string, "ch", Value::Object(None));
        ctx.set_field_by_name(xml_string, "offset", Value::Int(0));
        ctx.set_field_by_name(xml_string, "length", Value::Int(0));
        (scanner, entity, xml_string, chars)
    }

    fn new_qname(ctx: &mut MockNativeContext) -> ObjectRef {
        let class_id = ctx
            .ensure_class_initialized(XERCES_QNAME)
            .expect("QName class id");
        ctx.alloc_object(class_id, 4)
    }

    fn new_xerces_opti_node(
        ctx: &mut MockNativeContext,
        class_name: &str,
        prefix: Option<&str>,
        localpart: Option<&str>,
        rawname: Option<&str>,
        uri: Option<&str>,
        node_type: i32,
    ) -> ObjectRef {
        let class_id = ctx
            .ensure_class_initialized(class_name)
            .expect("opti class id");
        let node = ctx.alloc_object(class_id, 7);
        let prefix = prefix.map(|text| ctx.create_string(text));
        let localpart = localpart.map(|text| ctx.create_string(text));
        let rawname = rawname.map(|text| ctx.create_string(text));
        let uri = uri.map(|text| ctx.create_string(text));
        ctx.set_field_by_name(node, "prefix", Value::Object(prefix));
        ctx.set_field_by_name(node, "localpart", Value::Object(localpart));
        ctx.set_field_by_name(node, "rawname", Value::Object(rawname));
        ctx.set_field_by_name(node, "uri", Value::Object(uri));
        ctx.set_field_by_name(node, "nodeType", Value::Int(node_type));
        ctx.set_field_by_name(node, "hidden", Value::Int(0));
        ctx.set_field_by_name(node, "value", Value::Object(None));
        node
    }

    fn string_result(ctx: &MockNativeContext, value: Value) -> String {
        match value {
            Value::Object(Some(obj)) => ctx.read_string(obj).expect("string result"),
            other => panic!("unexpected result value: {other:?}"),
        }
    }

    fn java_string_hash(text: &str) -> i32 {
        text.encode_utf16().fold(0i32, |hash, unit| {
            hash.wrapping_mul(31).wrapping_add(unit as i32)
        })
    }

    #[test]
    fn essential_natives_include_xerces_xml_parser_intrinsics() {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        for (class_name, name, descriptor) in [
            (XERCES_XMLCHAR, "isNameStart", "(I)Z"),
            (XERCES_XMLCHAR, "isName", "(I)Z"),
            (XERCES_XMLCHAR, "isNCNameStart", "(I)Z"),
            (XERCES_XMLCHAR, "isNCName", "(I)Z"),
            (XERCES_XMLCHAR, "isSpace", "(I)Z"),
            (XML_LIMIT_ANALYZER, "addValue", "(ILjava/lang/String;I)V"),
            (
                XML_LIMIT_ANALYZER,
                "addValue",
                "(Ljdk/xml/internal/XMLSecurityManager$Limit;Ljava/lang/String;I)V",
            ),
            (XML_LIMIT_ANALYZER, "getValue", "(I)I"),
            (XML_LIMIT_ANALYZER, "getTotalValue", "(I)I"),
            (XML_LIMIT_ANALYZER, "getValueByIndex", "(I)I"),
            (
                XERCES_XSSIMPLE_TYPE_DECL,
                "normalize",
                "(Ljava/lang/String;S)Ljava/lang/String;",
            ),
            (
                XERCES_XSSIMPLE_TYPE_DECL,
                "normalize",
                "(Ljava/lang/Object;S)Ljava/lang/String;",
            ),
            (XERCES_XSD_KEY, "hashCode", "()I"),
            (XERCES_XSD_KEY, "equals", "(Ljava/lang/Object;)Z"),
            (
                XERCES_XML_ENTITY_SCANNER,
                "scanContent",
                "(Lcom/sun/org/apache/xerces/internal/xni/XMLString;)I",
            ),
            (
                XERCES_XML_ENTITY_SCANNER,
                "scanQName",
                "(Lcom/sun/org/apache/xerces/internal/xni/QName;Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z",
            ),
            (XERCES_XML_ENTITY_SCANNER, "skipSpaces", "()Z"),
            (
                XERCES_XML_ENTITY_SCANNER,
                "normalizeNewlines",
                "(SLcom/sun/org/apache/xerces/internal/xni/XMLString;ZZLcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z",
            ),
            (
                XERCES_XML_ENTITY_SCANNER,
                "checkEntityLimit",
                "(Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;Lcom/sun/xml/internal/stream/Entity$ScannedEntity;II)V",
            ),
            (XERCES_OPTI_NODE_IMPL, "getNodeName", "()Ljava/lang/String;"),
            (
                XERCES_OPTI_NODE_IMPL,
                "getNamespaceURI",
                "()Ljava/lang/String;",
            ),
            (XERCES_OPTI_NODE_IMPL, "getPrefix", "()Ljava/lang/String;"),
            (XERCES_OPTI_NODE_IMPL, "getLocalName", "()Ljava/lang/String;"),
            (XERCES_OPTI_NODE_IMPL, "getNodeType", "()S"),
            (XERCES_OPTI_NODE_IMPL, "getReadOnly", "()Z"),
            (
                XERCES_OPTI_ELEMENT_IMPL,
                "getTagName",
                "()Ljava/lang/String;",
            ),
            (XERCES_OPTI_ATTR_IMPL, "getName", "()Ljava/lang/String;"),
            (XERCES_OPTI_ATTR_IMPL, "getValue", "()Ljava/lang/String;"),
            (
                XERCES_OPTI_ATTR_IMPL,
                "getNodeValue",
                "()Ljava/lang/String;",
            ),
            (XERCES_OPTI_ATTR_IMPL, "getSpecified", "()Z"),
            (XERCES_OPTI_ATTR_IMPL, "isId", "()Z"),
            (XERCES_REGEX_RANGE_TOKEN, "sortRanges", "()V"),
        ] {
            assert!(
                registry.find(class_name, name, descriptor).is_some(),
                "{class_name}.{name}{descriptor} must be in the real-JDK essential registry"
            );
        }
    }

    #[test]
    fn xerces_xmlchar_reads_jdk_chars_masks() {
        let mut ctx = MockNativeContext::new();
        seed_xmlchar_table(&mut ctx);

        assert_eq!(
            native_xerces_xmlchar_is_name_start(&mut ctx, &[Value::Int('A' as i32)])
                .expect("isNameStart")
                .expect("value"),
            Value::Int(1)
        );
        assert_eq!(
            native_xerces_xmlchar_is_name_start(&mut ctx, &[Value::Int('-' as i32)])
                .expect("isNameStart")
                .expect("value"),
            Value::Int(0)
        );
        assert_eq!(
            native_xerces_xmlchar_is_ncname_start(&mut ctx, &[Value::Int(':' as i32)])
                .expect("isNCNameStart")
                .expect("value"),
            Value::Int(0)
        );
        assert_eq!(
            native_xerces_xmlchar_is_name(&mut ctx, &[Value::Int('-' as i32)])
                .expect("isName")
                .expect("value"),
            Value::Int(1)
        );
        assert_eq!(
            native_xerces_xmlchar_is_space(&mut ctx, &[Value::Int(' ' as i32)])
                .expect("isSpace")
                .expect("value"),
            Value::Int(1)
        );
        assert_eq!(
            native_xerces_xmlchar_is_name(&mut ctx, &[Value::Int(0x10000)])
                .expect("isName")
                .expect("value"),
            Value::Int(0)
        );
    }

    #[test]
    fn xml_limit_add_value_max_name_updates_values_and_total() {
        let mut ctx = MockNativeContext::new();
        let (analyzer, values, names, total_value, caches) = new_xml_limit_analyzer(&mut ctx);
        let entity = ctx.create_string("jpa-changelog");

        native_xml_limit_analyzer_add_value_index(
            &mut ctx,
            &[
                Value::Object(Some(analyzer)),
                Value::Int(XML_LIMIT_MAX_NAME),
                Value::Object(Some(entity)),
                Value::Int(23),
            ],
        )
        .expect("addValue");

        let slot = XML_LIMIT_MAX_NAME as usize;
        assert_eq!(ctx.get_array_element(values, slot), Value::Int(23));
        assert_eq!(ctx.get_array_element(total_value, slot), Value::Int(23));
        assert_eq!(ctx.get_array_element(names, slot), Value::Object(None));
        assert_eq!(ctx.get_array_element(caches, slot), Value::Object(None));
        assert_eq!(
            native_xml_limit_analyzer_get_value_index(
                &mut ctx,
                &[
                    Value::Object(Some(analyzer)),
                    Value::Int(XML_LIMIT_MAX_NAME)
                ],
            )
            .expect("getValue")
            .expect("value"),
            Value::Int(23)
        );
    }

    #[test]
    fn xml_limit_entity_replacement_get_value_reads_total_value() {
        let mut ctx = MockNativeContext::new();
        let (analyzer, values, _names, total_value, _caches) = new_xml_limit_analyzer(&mut ctx);

        native_xml_limit_analyzer_add_value_index(
            &mut ctx,
            &[
                Value::Object(Some(analyzer)),
                Value::Int(XML_LIMIT_ENTITY_REPLACEMENT),
                Value::Object(None),
                Value::Int(7),
            ],
        )
        .expect("addValue first");
        native_xml_limit_analyzer_add_value_index(
            &mut ctx,
            &[
                Value::Object(Some(analyzer)),
                Value::Int(XML_LIMIT_ENTITY_REPLACEMENT),
                Value::Object(None),
                Value::Int(5),
            ],
        )
        .expect("addValue second");

        let slot = XML_LIMIT_ENTITY_REPLACEMENT as usize;
        assert_eq!(ctx.get_array_element(values, slot), Value::Int(0));
        assert_eq!(ctx.get_array_element(total_value, slot), Value::Int(12));
        assert_eq!(
            native_xml_limit_analyzer_get_value_index(
                &mut ctx,
                &[
                    Value::Object(Some(analyzer)),
                    Value::Int(XML_LIMIT_ENTITY_REPLACEMENT),
                ],
            )
            .expect("getValue")
            .expect("value"),
            Value::Int(12)
        );
    }

    #[test]
    fn xssimple_type_static_normalize_matches_xerces_whitespace_rules() {
        let mut ctx = MockNativeContext::new();
        let content = ctx.create_string(" a\t b\nc ");

        assert_eq!(
            native_xssimple_type_normalize_string(
                &mut ctx,
                &[Value::Object(Some(content)), Value::Int(0)]
            )
            .expect("normalize preserve")
            .expect("preserve value"),
            Value::Object(Some(content))
        );

        let replaced = native_xssimple_type_normalize_string(
            &mut ctx,
            &[Value::Object(Some(content)), Value::Int(1)],
        )
        .expect("normalize replace")
        .expect("replace value");
        // XML Schema's `replace` facet maps TAB/LF/CR to U+0020 and does
        // not collapse pre-existing or newly adjacent spaces.
        assert_eq!(string_result(&ctx, replaced), " a  b c ");

        let collapsed = native_xssimple_type_normalize_string(
            &mut ctx,
            &[Value::Object(Some(content)), Value::Int(2)],
        )
        .expect("normalize collapse")
        .expect("collapse value");
        assert_eq!(string_result(&ctx, collapsed), "a b c");
    }

    #[test]
    fn xssimple_type_object_normalize_honors_datatype_default_without_pattern_facet() {
        let mut ctx = MockNativeContext::new();
        let decl = new_xssimple_type_decl(&mut ctx, 2, 0);
        let value = ctx.create_string(" \t token \n ");

        let trimmed = native_xssimple_type_normalize_object(
            &mut ctx,
            &[
                Value::Object(Some(decl)),
                Value::Object(Some(value)),
                Value::Int(2),
            ],
        )
        .expect("normalize object")
        .expect("object value");
        assert_eq!(string_result(&ctx, trimmed), "token");
    }

    #[test]
    fn xssimple_type_object_normalize_collapses_stringbuffer_in_place() {
        let mut ctx = MockNativeContext::new();
        let decl = new_xssimple_type_decl(&mut ctx, 1, XSSIMPLE_FACET_PATTERN);
        let buffer = new_string_buffer(&mut ctx, "  a\t b\nc  ");

        let normalized = native_xssimple_type_normalize_object(
            &mut ctx,
            &[
                Value::Object(Some(decl)),
                Value::Object(Some(buffer)),
                Value::Int(2),
            ],
        )
        .expect("normalize stringbuffer")
        .expect("stringbuffer value");

        assert_eq!(string_result(&ctx, normalized), "a b c");
        assert_eq!(
            String::from_utf16(&crate::lang_string::sb_read_chars(&ctx, buffer))
                .expect("buffer chars"),
            "a b c"
        );
    }

    #[test]
    fn xsd_key_hash_and_equals_match_xerces_cache_key_semantics() {
        let mut ctx = MockNativeContext::new();
        let refer_ns = ctx.create_string("urn:keycloak:test");
        let same_refer_ns = refer_ns;
        let equal_text_different_refer_ns = ctx.create_string_uninterned("urn:keycloak:test");
        let key = new_xsd_key(&mut ctx, Some("dbchangelog-3.1.xsd"), Some(refer_ns));
        let same = new_xsd_key(&mut ctx, Some("dbchangelog-3.1.xsd"), Some(same_refer_ns));
        let different_ns_identity = new_xsd_key(
            &mut ctx,
            Some("dbchangelog-3.1.xsd"),
            Some(equal_text_different_refer_ns),
        );
        let null_system = new_xsd_key(&mut ctx, None, Some(refer_ns));

        assert_eq!(
            native_xsd_key_hash_code(&mut ctx, &[Value::Object(Some(key))])
                .expect("hashCode")
                .expect("hash value"),
            Value::Int(java_string_hash("urn:keycloak:test"))
        );
        assert_eq!(
            native_xsd_key_equals(
                &mut ctx,
                &[Value::Object(Some(key)), Value::Object(Some(same))]
            )
            .expect("equals same")
            .expect("same value"),
            Value::Int(1)
        );
        assert_eq!(
            native_xsd_key_equals(
                &mut ctx,
                &[
                    Value::Object(Some(key)),
                    Value::Object(Some(different_ns_identity)),
                ],
            )
            .expect("equals different ns")
            .expect("different ns value"),
            Value::Int(0)
        );
        assert_eq!(
            native_xsd_key_equals(
                &mut ctx,
                &[Value::Object(Some(null_system)), Value::Object(Some(same))]
            )
            .expect("equals null system")
            .expect("null system value"),
            Value::Int(0)
        );
    }

    #[test]
    fn xml_entity_scanner_scan_content_updates_entity_and_xmlstring_slice() {
        let mut ctx = MockNativeContext::new();
        seed_xmlchar_content_table(&mut ctx);
        let (scanner, entity, xml_string, chars) = new_entity_scanner_fixture(&mut ctx, "abc<tail");

        assert_eq!(
            native_xml_entity_scanner_scan_content(
                &mut ctx,
                &[
                    Value::Object(Some(scanner)),
                    Value::Object(Some(xml_string))
                ]
            )
            .expect("scanContent")
            .expect("scan result"),
            Value::Int('<' as i32)
        );
        assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(3));
        assert_eq!(ctx.get_field_by_name(entity, "columnNumber"), Value::Int(3));
        assert_eq!(
            ctx.get_field_by_name(xml_string, "ch"),
            Value::Object(Some(chars))
        );
        assert_eq!(ctx.get_field_by_name(xml_string, "offset"), Value::Int(0));
        assert_eq!(ctx.get_field_by_name(xml_string, "length"), Value::Int(3));
    }

    #[test]
    fn xml_entity_scanner_scan_qname_sets_qname_and_updates_limits() {
        let mut ctx = MockNativeContext::new();
        seed_xmlchar_table(&mut ctx);
        let (analyzer, values, names, total_value, _caches) = new_xml_limit_analyzer(&mut ctx);
        let (scanner, entity, _xml_string, _chars) =
            new_entity_scanner_fixture(&mut ctx, "xsd:element ");
        let qname = new_qname(&mut ctx);
        let entity_name = ctx.create_string("liquibase-entity");
        ctx.set_field_by_name(scanner, "fLimitAnalyzer", Value::Object(Some(analyzer)));
        ctx.set_field_by_name(entity, "isGE", Value::Int(1));
        ctx.set_field_by_name(entity, "name", Value::Object(Some(entity_name)));

        assert_eq!(
            native_xml_entity_scanner_scan_qname(
                &mut ctx,
                &[
                    Value::Object(Some(scanner)),
                    Value::Object(Some(qname)),
                    Value::Object(None),
                ],
            )
            .expect("scanQName")
            .expect("scan result"),
            Value::Int(1)
        );

        assert_eq!(ctx.get_field_by_name(scanner, "offset"), Value::Int(0));
        assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(11));
        assert_eq!(
            ctx.get_field_by_name(entity, "columnNumber"),
            Value::Int(11)
        );
        assert_eq!(
            string_result(&ctx, ctx.get_field_by_name(qname, "prefix")),
            "xsd"
        );
        assert_eq!(
            string_result(&ctx, ctx.get_field_by_name(qname, "localpart")),
            "element"
        );
        assert_eq!(
            string_result(&ctx, ctx.get_field_by_name(qname, "rawname")),
            "xsd:element"
        );
        assert_eq!(ctx.get_field_by_name(qname, "uri"), Value::Object(None));
        assert_eq!(
            ctx.get_array_element(values, XML_LIMIT_MAX_NAME as usize),
            Value::Int(7)
        );
        assert_eq!(
            ctx.get_array_element(values, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
            Value::Int(11)
        );
        assert_eq!(
            ctx.get_array_element(total_value, XML_LIMIT_TOTAL_ENTITY_SIZE as usize),
            Value::Int(11)
        );
        assert_eq!(
            ctx.get_array_element(names, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
            Value::Object(Some(entity_name))
        );
    }

    #[test]
    fn xml_entity_scanner_skip_spaces_consumes_ascii_whitespace() {
        let mut ctx = MockNativeContext::new();
        let (scanner, entity, _xml_string, _chars) = new_entity_scanner_fixture(&mut ctx, " \tX");

        assert_eq!(
            native_xml_entity_scanner_skip_spaces(&mut ctx, &[Value::Object(Some(scanner))])
                .expect("skipSpaces")
                .expect("skip result"),
            Value::Int(1)
        );
        assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(2));
        assert_eq!(ctx.get_field_by_name(entity, "columnNumber"), Value::Int(2));
        assert_eq!(ctx.get_field_by_name(entity, "lineNumber"), Value::Int(1));
        assert_eq!(ctx.get_field_by_name(scanner, "offset"), Value::Int(1));
    }

    #[test]
    fn xml_entity_scanner_normalize_newlines_sets_xmlstring_at_boundary() {
        let mut ctx = MockNativeContext::new();
        let (scanner, entity, xml_string, chars) = new_entity_scanner_fixture(&mut ctx, "\nX");

        assert_eq!(
            native_xml_entity_scanner_normalize_newlines(
                &mut ctx,
                &[
                    Value::Object(Some(scanner)),
                    Value::Int(1),
                    Value::Object(Some(xml_string)),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Object(None),
                ],
            )
            .expect("normalizeNewlines")
            .expect("normalize result"),
            Value::Int(1)
        );
        assert_eq!(ctx.get_field_by_name(scanner, "offset"), Value::Int(0));
        assert_eq!(ctx.get_field_by_name(scanner, "newlines"), Value::Int(1));
        assert_eq!(ctx.get_field_by_name(scanner, "counted"), Value::Int(0));
        assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(1));
        assert_eq!(ctx.get_field_by_name(entity, "lineNumber"), Value::Int(2));
        assert_eq!(ctx.get_field_by_name(entity, "columnNumber"), Value::Int(1));
        assert_eq!(ctx.get_array_element(chars, 0), Value::Int('\n' as i32));
        assert_eq!(
            ctx.get_field_by_name(xml_string, "ch"),
            Value::Object(Some(chars))
        );
        assert_eq!(ctx.get_field_by_name(xml_string, "offset"), Value::Int(0));
        assert_eq!(ctx.get_field_by_name(xml_string, "length"), Value::Int(1));
    }

    #[test]
    fn xml_entity_scanner_check_entity_limit_updates_general_entity_accounting() {
        let mut ctx = MockNativeContext::new();
        let (analyzer, values, names, total_value, _caches) = new_xml_limit_analyzer(&mut ctx);
        let (scanner, entity, _xml_string, _chars) = new_entity_scanner_fixture(&mut ctx, "abc");
        let entity_name = ctx.create_string("liquibase-entity");
        ctx.set_field_by_name(scanner, "fLimitAnalyzer", Value::Object(Some(analyzer)));
        ctx.set_field_by_name(entity, "isGE", Value::Int(1));
        ctx.set_field_by_name(entity, "name", Value::Object(Some(entity_name)));

        native_xml_entity_scanner_check_entity_limit(
            &mut ctx,
            &[
                Value::Object(Some(scanner)),
                Value::Object(None),
                Value::Object(Some(entity)),
                Value::Int(0),
                Value::Int(7),
            ],
        )
        .expect("checkEntityLimit");

        assert_eq!(
            ctx.get_array_element(values, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
            Value::Int(7)
        );
        assert_eq!(
            ctx.get_array_element(total_value, XML_LIMIT_TOTAL_ENTITY_SIZE as usize),
            Value::Int(7)
        );
        assert_eq!(
            ctx.get_array_element(names, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
            Value::Object(Some(entity_name))
        );
    }

    #[test]
    fn xerces_opti_dom_getters_return_backing_fields() {
        let mut ctx = MockNativeContext::new();
        let node = new_xerces_opti_node(
            &mut ctx,
            XERCES_OPTI_NODE_IMPL,
            Some("xsd"),
            Some("element"),
            Some("xsd:element"),
            Some("http://www.w3.org/2001/XMLSchema"),
            1,
        );
        ctx.set_field_by_name(node, "hidden", Value::Int(1));

        let node_name =
            native_xerces_opti_node_get_node_name(&mut ctx, &[Value::Object(Some(node))])
                .expect("getNodeName")
                .expect("node name");
        assert_eq!(string_result(&ctx, node_name), "xsd:element");
        let namespace =
            native_xerces_opti_node_get_namespace_uri(&mut ctx, &[Value::Object(Some(node))])
                .expect("getNamespaceURI")
                .expect("namespace");
        assert_eq!(
            string_result(&ctx, namespace),
            "http://www.w3.org/2001/XMLSchema"
        );
        let prefix = native_xerces_opti_node_get_prefix(&mut ctx, &[Value::Object(Some(node))])
            .expect("getPrefix")
            .expect("prefix");
        assert_eq!(string_result(&ctx, prefix), "xsd");
        let local_name =
            native_xerces_opti_node_get_local_name(&mut ctx, &[Value::Object(Some(node))])
                .expect("getLocalName")
                .expect("local name");
        assert_eq!(string_result(&ctx, local_name), "element");
        assert_eq!(
            native_xerces_opti_node_get_node_type(&mut ctx, &[Value::Object(Some(node))])
                .expect("getNodeType")
                .expect("node type"),
            Value::Int(1)
        );
        assert_eq!(
            native_xerces_opti_node_get_read_only(&mut ctx, &[Value::Object(Some(node))])
                .expect("getReadOnly")
                .expect("read only"),
            Value::Int(1)
        );

        let attr = new_xerces_opti_node(
            &mut ctx,
            XERCES_OPTI_ATTR_IMPL,
            None,
            Some("name"),
            Some("name"),
            None,
            2,
        );
        let attr_value = ctx.create_string("xs:string");
        ctx.set_field_by_name(attr, "value", Value::Object(Some(attr_value)));

        let attr_name = native_xerces_opti_attr_get_name(&mut ctx, &[Value::Object(Some(attr))])
            .expect("getName")
            .expect("attr name");
        assert_eq!(string_result(&ctx, attr_name), "name");
        let attr_value =
            native_xerces_opti_attr_get_node_value(&mut ctx, &[Value::Object(Some(attr))])
                .expect("getNodeValue")
                .expect("attr value");
        assert_eq!(string_result(&ctx, attr_value), "xs:string");
        assert_eq!(
            native_xerces_opti_attr_get_specified(&mut ctx, &[Value::Object(Some(attr))])
                .expect("getSpecified")
                .expect("specified"),
            Value::Int(1)
        );
        assert_eq!(
            native_xerces_opti_attr_is_id(&mut ctx, &[Value::Object(Some(attr))])
                .expect("isId")
                .expect("id"),
            Value::Int(0)
        );
    }

    #[test]
    fn xerces_regex_range_token_sort_ranges_orders_pairs() {
        let mut ctx = MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized(XERCES_REGEX_RANGE_TOKEN)
            .expect("RangeToken class id");
        let token = ctx.alloc_object(class_id, 3);
        let ranges = ctx.new_array(ArrayElementType::Int, 6);
        for (slot, value) in [5, 6, 1, 2, 5, 5].into_iter().enumerate() {
            ctx.set_array_element(ranges, slot, Value::Int(value));
        }
        ctx.set_field_by_name(token, "ranges", Value::Object(Some(ranges)));
        ctx.set_field_by_name(token, "sorted", Value::Int(0));
        ctx.set_field_by_name(token, "compacted", Value::Int(0));

        native_xerces_regex_range_token_sort_ranges(&mut ctx, &[Value::Object(Some(token))])
            .expect("sortRanges");

        let sorted: Vec<i32> = (0..6)
            .map(|slot| ctx.get_array_element(ranges, slot).as_int().unwrap())
            .collect();
        assert_eq!(sorted, vec![1, 2, 5, 5, 5, 6]);
        assert_eq!(ctx.get_field_by_name(token, "sorted"), Value::Int(1));
        assert_eq!(ctx.get_field_by_name(token, "compacted"), Value::Int(0));
    }
}
