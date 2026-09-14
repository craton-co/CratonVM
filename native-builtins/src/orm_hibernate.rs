// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Hibernate ORM shims (hibernate-models annotation usage, NavigablePath, bytecode-enhancement helpers).
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

fn hibernate_optional_get(
    ctx: &mut dyn NativeContext,
    optional: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let optional_pin = ctx.pin_native_root(optional);
    let present = match ctx.invoke_virtual(optional, "isPresent", "()Z", &[])? {
        Some(Value::Int(v)) => v != 0,
        _ => false,
    };
    let optional = ctx.read_native_pin(optional_pin, optional);
    if !present {
        ctx.unpin_native_roots(optional_pin);
        return Ok(None);
    }
    let result = ctx.invoke_virtual(optional, "get", "()Ljava/lang/Object;", &[]);
    ctx.unpin_native_roots(optional_pin);
    match result? {
        Some(Value::Object(obj)) => Ok(obj),
        _ => Ok(None),
    }
}

fn hibernate_annotated_element_has_direct_annotation(
    ctx: &mut dyn NativeContext,
    element: ObjectRef,
    annotation_type: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let element_pin = ctx.pin_native_root(element);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let result = ctx.invoke_virtual(
        element,
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(annotation_type))],
    );
    ctx.unpin_native_roots(element_pin);
    match result? {
        Some(Value::Object(Some(_))) => Ok(true),
        _ => Ok(false),
    }
}

fn hibernate_testing_util_has_effective_annotation_pinned(
    ctx: &mut dyn NativeContext,
    mut context: ObjectRef,
    context_pin: usize,
    mut annotation_type: ObjectRef,
    annotation_pin: usize,
) -> Result<bool, MethodCallFailed> {
    context = ctx.read_native_pin(context_pin, context);
    annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let element_optional =
        match ctx.invoke_virtual(context, "getElement", "()Ljava/util/Optional;", &[])? {
            Some(Value::Object(Some(optional))) => optional,
            _ => return Ok(false),
        };

    context = ctx.read_native_pin(context_pin, context);
    annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let Some(element) = hibernate_optional_get(ctx, element_optional)? else {
        return Ok(false);
    };
    annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    if hibernate_annotated_element_has_direct_annotation(ctx, element, annotation_type)? {
        return Ok(true);
    }

    context = ctx.read_native_pin(context_pin, context);
    let test_instance_optional =
        match ctx.invoke_virtual(context, "getTestInstance", "()Ljava/util/Optional;", &[])? {
            Some(Value::Object(Some(optional))) => optional,
            _ => return Ok(false),
        };
    if hibernate_optional_get(ctx, test_instance_optional)?.is_none() {
        return Ok(false);
    }

    context = ctx.read_native_pin(context_pin, context);
    annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let test_class =
        match ctx.invoke_virtual(context, "getRequiredTestClass", "()Ljava/lang/Class;", &[])? {
            Some(Value::Object(Some(class_mirror))) => class_mirror,
            _ => return Ok(false),
        };
    annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    hibernate_annotated_element_has_direct_annotation(ctx, test_class, annotation_type)
}

fn native_hibernate_testing_util_has_effective_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let context = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    let context_pin = ctx.pin_native_root(context);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let result = hibernate_testing_util_has_effective_annotation_pinned(
        ctx,
        context,
        context_pin,
        annotation_type,
        annotation_pin,
    );
    ctx.unpin_native_roots(context_pin);
    Ok(Some(antlr_bool(result?)))
}

fn hibernate_annotation_usage_map(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.invoke_virtual(target, "getUsageMap", "()Ljava/util/Map;", &[])? {
        Some(Value::Object(Some(map))) => Ok(map),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("AnnotationTargetSupport.getUsageMap returned null".to_string()),
        }
        .into()),
    }
}

fn hibernate_is_orm_annotation_descriptor(ctx: &dyn NativeContext, target: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(target))
        .as_deref()
        == Some(HIBERNATE_ORM_ANNOTATION_DESCRIPTOR)
}

fn hibernate_method_result_is_null(value: &Option<Value>) -> bool {
    matches!(value, None | Some(Value::Object(None)))
}

fn hibernate_map_contains_key(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    key: ObjectRef,
) -> MethodCallResult {
    ctx.invoke_virtual(
        map,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(key))],
    )
}

fn hibernate_map_get(ctx: &mut dyn NativeContext, map: ObjectRef, key: Value) -> MethodCallResult {
    ctx.invoke_virtual(map, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[key])
}

fn hibernate_descriptor_annotation_type(
    ctx: &mut dyn NativeContext,
    descriptor: ObjectRef,
) -> MethodCallResult {
    ctx.invoke_virtual(descriptor, "getAnnotationType", "()Ljava/lang/Class;", &[])
}

fn hibernate_descriptor_annotation_type_key(
    ctx: &mut dyn NativeContext,
    descriptor: ObjectRef,
) -> Result<Value, MethodCallFailed> {
    match hibernate_descriptor_annotation_type(ctx, descriptor)? {
        Some(Value::Object(annotation_type)) => Ok(Value::Object(annotation_type)),
        _ => Ok(Value::Object(None)),
    }
}

fn hibernate_find_annotation_usage(
    ctx: &mut dyn NativeContext,
    descriptor: ObjectRef,
    map: ObjectRef,
) -> MethodCallResult {
    let key = hibernate_descriptor_annotation_type_key(ctx, descriptor)?;
    hibernate_map_get(ctx, map, key)
}

fn hibernate_get_direct_annotation_usage(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    annotation_type: ObjectRef,
) -> MethodCallResult {
    if hibernate_is_orm_annotation_descriptor(ctx, target) {
        return Ok(Some(Value::Object(None)));
    }

    let target_pin = ctx.pin_native_root(target);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let result = (|| {
        let target = ctx.read_native_pin(target_pin, target);
        let map = hibernate_annotation_usage_map(ctx, target)?;
        let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
        hibernate_map_get(ctx, map, Value::Object(Some(annotation_type)))
    })();
    ctx.unpin_native_roots(target_pin);
    result
}

fn hibernate_object_or_null(value: Option<Value>) -> Option<ObjectRef> {
    match value {
        Some(Value::Object(obj)) => obj,
        _ => None,
    }
}

fn hibernate_throw_annotation_access_exception(
    ctx: &mut dyn NativeContext,
    descriptor: ObjectRef,
) -> MethodCallFailed {
    let annotation_name = hibernate_descriptor_annotation_type(ctx, descriptor)
        .ok()
        .and_then(hibernate_object_or_null)
        .and_then(|annotation_type| {
            ctx.invoke_virtual(annotation_type, "getName", "()Ljava/lang/String;", &[])
                .ok()
                .and_then(hibernate_object_or_null)
        })
        .and_then(|name| ctx.read_string(name))
        .unwrap_or_else(|| "<unknown>".to_string());
    let message_text = format!("Multiple {annotation_name} annotations found");
    let message = ctx.create_string(&message_text);
    match ctx.new_object_initialized(
        "org/hibernate/models/AnnotationAccessException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalStateException {
            message: message_text,
        }
        .into(),
    }
}

fn hibernate_get_usage_by_descriptor(
    ctx: &mut dyn NativeContext,
    descriptor: ObjectRef,
    map: ObjectRef,
    model_context: Option<ObjectRef>,
) -> MethodCallResult {
    let direct = hibernate_find_annotation_usage(ctx, descriptor, map)?;
    if !matches!(direct, None | Some(Value::Object(None))) {
        return Ok(direct);
    }

    let container = match ctx.invoke_virtual(
        descriptor,
        "getRepeatableContainer",
        "()Lorg/hibernate/models/spi/AnnotationDescriptor;",
        &[],
    )? {
        Some(Value::Object(Some(container))) => container,
        _ => return Ok(Some(Value::Object(None))),
    };
    let container_usage = hibernate_find_annotation_usage(ctx, container, map)?;
    let Some(container_usage) = hibernate_object_or_null(container_usage) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(model_context) = model_context else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };

    let repeated_values = match ctx.invoke(
        HIBERNATE_ANNOTATION_USAGE_HELPER,
        "extractRepeatedValues",
        "(Ljava/lang/annotation/Annotation;Lorg/hibernate/models/spi/AnnotationDescriptor;Lorg/hibernate/models/spi/ModelsContext;)[Ljava/lang/annotation/Annotation;",
        &[
            Value::Object(Some(container_usage)),
            Value::Object(Some(container)),
            Value::Object(Some(model_context)),
        ],
    )? {
        Some(Value::Object(Some(values))) => values,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(repeated_values);
    if len == 0 {
        return Ok(Some(Value::Object(None)));
    }
    if len > 1 {
        return Err(hibernate_throw_annotation_access_exception(ctx, descriptor));
    }
    Ok(Some(ctx.get_array_element(repeated_values, 0)))
}

fn hibernate_get_usage_by_class(
    ctx: &mut dyn NativeContext,
    annotation_type: ObjectRef,
    map: ObjectRef,
    model_context: ObjectRef,
) -> MethodCallResult {
    let registry = match ctx.invoke_virtual(
        model_context,
        "getAnnotationDescriptorRegistry",
        "()Lorg/hibernate/models/spi/AnnotationDescriptorRegistry;",
        &[],
    )? {
        Some(Value::Object(Some(registry))) => registry,
        _ => return Ok(Some(Value::Object(None))),
    };
    let descriptor = match ctx.invoke_virtual(
        registry,
        "getDescriptor",
        "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
        &[Value::Object(Some(annotation_type))],
    )? {
        Some(Value::Object(Some(descriptor))) => descriptor,
        _ => return Ok(Some(Value::Object(None))),
    };
    hibernate_get_usage_by_descriptor(ctx, descriptor, map, Some(model_context))
}

fn native_hibernate_annotation_target_has_direct_annotation_usage(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    let this_pin = ctx.pin_native_root(this);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let map_result = hibernate_annotation_usage_map(ctx, this);
    let map = match map_result {
        Ok(map) => map,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let result = hibernate_map_contains_key(ctx, map, annotation_type);
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_hibernate_annotation_target_get_direct_annotation_usage(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    hibernate_get_direct_annotation_usage(ctx, this, annotation_type)
}

fn native_hibernate_annotation_target_get_annotation_usage_by_descriptor(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let descriptor = obj_arg(args, 1)?;
    let model_context = match args.get(2) {
        Some(Value::Object(model_context)) => *model_context,
        _ => None,
    };
    let this_pin = ctx.pin_native_root(this);
    let descriptor_pin = ctx.pin_native_root(descriptor);
    let model_context_pin = model_context.map(|obj| ctx.pin_native_root(obj));
    let map = match hibernate_annotation_usage_map(ctx, this) {
        Ok(map) => map,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    let descriptor = ctx.read_native_pin(descriptor_pin, descriptor);
    let model_context = match (model_context_pin, model_context) {
        (Some(pin), Some(fallback)) => Some(ctx.read_native_pin(pin, fallback)),
        _ => model_context,
    };
    let result = hibernate_get_usage_by_descriptor(ctx, descriptor, map, model_context);
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_hibernate_annotation_target_get_annotation_usage_by_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    let model_context = obj_arg(args, 2)?;
    let this_pin = ctx.pin_native_root(this);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let model_context_pin = ctx.pin_native_root(model_context);
    let map = match hibernate_annotation_usage_map(ctx, this) {
        Ok(map) => map,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let model_context = ctx.read_native_pin(model_context_pin, model_context);
    let result = hibernate_get_usage_by_class(ctx, annotation_type, map, model_context);
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_hibernate_annotation_target_locate_annotation_usage(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    let model_context = obj_arg(args, 2)?;
    let this_pin = ctx.pin_native_root(this);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let model_context_pin = ctx.pin_native_root(model_context);

    let result = (|| {
        let this = ctx.read_native_pin(this_pin, this);
        let map = hibernate_annotation_usage_map(ctx, this)?;
        let map_pin = ctx.pin_native_root(map);

        let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
        let model_context = ctx.read_native_pin(model_context_pin, model_context);
        let direct = hibernate_get_usage_by_class(ctx, annotation_type, map, model_context)?;
        if !hibernate_method_result_is_null(&direct) {
            return Ok(direct);
        }

        let model_context = ctx.read_native_pin(model_context_pin, model_context);
        let registry = match ctx.invoke_virtual(
            model_context,
            "getAnnotationDescriptorRegistry",
            "()Lorg/hibernate/models/spi/AnnotationDescriptorRegistry;",
            &[],
        )? {
            Some(Value::Object(Some(registry))) => registry,
            _ => return Ok(Some(Value::Object(None))),
        };
        let registry_pin = ctx.pin_native_root(registry);

        let map = ctx.read_native_pin(map_pin, map);
        let entries = match ctx.invoke_virtual(map, "entrySet", "()Ljava/util/Set;", &[])? {
            Some(Value::Object(Some(entries))) => entries,
            _ => return Ok(Some(Value::Object(None))),
        };
        let entries_pin = ctx.pin_native_root(entries);
        let entries = ctx.read_native_pin(entries_pin, entries);
        let iterator =
            match ctx.invoke_virtual(entries, "iterator", "()Ljava/util/Iterator;", &[])? {
                Some(Value::Object(Some(iterator))) => iterator,
                _ => return Ok(Some(Value::Object(None))),
            };
        let iterator_pin = ctx.pin_native_root(iterator);

        loop {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = match ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if !has_next {
                break;
            }

            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let entry = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(entry))) => entry,
                _ => break,
            };
            let annotation_usage =
                match ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[])? {
                    Some(Value::Object(Some(annotation_usage))) => annotation_usage,
                    _ => continue,
                };
            let usage_pin = ctx.pin_native_root(annotation_usage);
            let annotation_usage = ctx.read_native_pin(usage_pin, annotation_usage);
            let used_annotation_type = match ctx.invoke_virtual(
                annotation_usage,
                "annotationType",
                "()Ljava/lang/Class;",
                &[],
            )? {
                Some(Value::Object(Some(annotation_type))) => annotation_type,
                _ => {
                    ctx.unpin_native_roots(usage_pin);
                    continue;
                }
            };
            let used_annotation_pin = ctx.pin_native_root(used_annotation_type);

            let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
            let same_type = match ctx.invoke_virtual(
                annotation_type,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(used_annotation_type))],
            )? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if same_type {
                ctx.unpin_native_roots(usage_pin);
                continue;
            }

            let registry = ctx.read_native_pin(registry_pin, registry);
            let used_annotation_type =
                ctx.read_native_pin(used_annotation_pin, used_annotation_type);
            let descriptor = match ctx.invoke_virtual(
                registry,
                "getDescriptor",
                "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
                &[Value::Object(Some(used_annotation_type))],
            )? {
                Some(Value::Object(Some(descriptor))) => descriptor,
                _ => {
                    ctx.unpin_native_roots(usage_pin);
                    continue;
                }
            };

            let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
            let result = hibernate_get_direct_annotation_usage(ctx, descriptor, annotation_type)?;
            ctx.unpin_native_roots(usage_pin);
            if !hibernate_method_result_is_null(&result) {
                return Ok(result);
            }
        }

        Ok(Some(Value::Object(None)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

fn hibernate_repeatable_class_mirror(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let _ = ctx.load_class("java/lang/annotation/Repeatable")?;
    match ctx.class_id_by_name("java/lang/annotation/Repeatable") {
        Some(class_id) => Ok(ctx.get_class_mirror(class_id)),
        None => Err(RuntimeError::IllegalStateException {
            message: "java.lang.annotation.Repeatable is not loaded".to_string(),
        }
        .into()),
    }
}

fn hibernate_descriptor_registry_map(
    ctx: &mut dyn NativeContext,
    registry: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(registry, "descriptorMap") {
        Value::Object(Some(map)) => Ok(map),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("AnnotationDescriptorRegistryStandard.descriptorMap is null".to_string()),
        }
        .into()),
    }
}

fn hibernate_descriptor_registry_models_context(
    ctx: &mut dyn NativeContext,
    registry: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(registry, "modelsContext") {
        Value::Object(Some(models_context)) => Ok(models_context),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("AnnotationDescriptorRegistryStandard.modelsContext is null".to_string()),
        }
        .into()),
    }
}

fn hibernate_repeatable_container_descriptor(
    ctx: &mut dyn NativeContext,
    registry: ObjectRef,
    annotation_type: ObjectRef,
    depth: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let repeatable_class = hibernate_repeatable_class_mirror(ctx)?;
    let repeatable = match ctx.invoke_virtual(
        annotation_type,
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(repeatable_class))],
    )? {
        Some(Value::Object(Some(repeatable))) => repeatable,
        _ => return Ok(None),
    };
    let container_type =
        match ctx.invoke_virtual(repeatable, "value", "()Ljava/lang/Class;", &[])? {
            Some(Value::Object(Some(container_type))) => container_type,
            _ => return Ok(None),
        };
    match hibernate_descriptor_registry_get_descriptor_inner(
        ctx,
        registry,
        container_type,
        depth + 1,
    )? {
        Some(Value::Object(container_descriptor)) => Ok(container_descriptor),
        _ => Ok(None),
    }
}

fn hibernate_descriptor_registry_get_descriptor_inner(
    ctx: &mut dyn NativeContext,
    registry: ObjectRef,
    annotation_type: ObjectRef,
    depth: usize,
) -> MethodCallResult {
    if depth > 8 {
        return Err(RuntimeError::IllegalStateException {
            message: "repeatable annotation descriptor recursion exceeded".to_string(),
        }
        .into());
    }

    let registry_pin = ctx.pin_native_root(registry);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let result = (|| {
        let registry = ctx.read_native_pin(registry_pin, registry);
        let map = hibernate_descriptor_registry_map(ctx, registry)?;
        let map_pin = ctx.pin_native_root(map);
        let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
        let existing = hibernate_map_get(ctx, map, Value::Object(Some(annotation_type)))?;
        if !hibernate_method_result_is_null(&existing) {
            return Ok(existing);
        }

        let registry = ctx.read_native_pin(registry_pin, registry);
        let models_context = hibernate_descriptor_registry_models_context(ctx, registry)?;
        let models_context_pin = ctx.pin_native_root(models_context);
        let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
        let container_descriptor =
            hibernate_repeatable_container_descriptor(ctx, registry, annotation_type, depth)?;
        let container_pin = container_descriptor.map(|obj| ctx.pin_native_root(obj));

        let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
        let models_context = ctx.read_native_pin(models_context_pin, models_context);
        let container_descriptor = match (container_descriptor, container_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        let descriptor = match ctx.new_object_initialized(
            "org/hibernate/models/internal/StandardAnnotationDescriptor",
            "(Ljava/lang/Class;Lorg/hibernate/models/spi/AnnotationDescriptor;Lorg/hibernate/models/spi/ModelsContext;)V",
            &[
                Value::Object(Some(annotation_type)),
                Value::Object(container_descriptor),
                Value::Object(Some(models_context)),
            ],
        )? {
            Some(Value::Object(Some(descriptor))) => descriptor,
            _ => return Ok(Some(Value::Object(None))),
        };
        let descriptor_pin = ctx.pin_native_root(descriptor);

        let map = ctx.read_native_pin(map_pin, map);
        let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
        let descriptor = ctx.read_native_pin(descriptor_pin, descriptor);
        ctx.invoke_virtual(
            map,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(annotation_type)),
                Value::Object(Some(descriptor)),
            ],
        )?;
        let descriptor = ctx.read_native_pin(descriptor_pin, descriptor);
        Ok(Some(Value::Object(Some(descriptor))))
    })();
    ctx.unpin_native_roots(registry_pin);
    result
}

fn native_hibernate_annotation_descriptor_registry_get_descriptor(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let registry = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    hibernate_descriptor_registry_get_descriptor_inner(ctx, registry, annotation_type, 0)
}

fn hibernate_object_hash(ctx: &mut dyn NativeContext, obj: Option<ObjectRef>) -> MethodCallResult {
    match obj {
        Some(obj) => ctx.invoke_virtual(obj, "hashCode", "()I", &[]),
        None => Ok(Some(Value::Int(0))),
    }
}

fn hibernate_objects_equal(
    ctx: &mut dyn NativeContext,
    left: Option<ObjectRef>,
    right: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(a), Some(b)) if a == b => Ok(true),
        (Some(a), Some(b)) => match ctx.invoke_virtual(
            a,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(b))],
        )? {
            Some(Value::Int(v)) => Ok(v != 0),
            _ => Ok(false),
        },
        _ => Ok(false),
    }
}

fn hibernate_association_key_components(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> (Option<ObjectRef>, Option<ObjectRef>) {
    let table = match ctx.get_field_by_name(this, "table") {
        Value::Object(table) => table,
        _ => None,
    };
    let columns = match ctx.get_field_by_name(this, "columns") {
        Value::Object(columns) => columns,
        _ => None,
    };
    (table, columns)
}

fn native_hibernate_association_key_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (table, columns) = hibernate_association_key_components(ctx, this);
    let table_pin = table.map(|obj| ctx.pin_native_root(obj));
    let columns_pin = columns.map(|obj| ctx.pin_native_root(obj));
    let result = (|| {
        let table = match (table, table_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        let table_hash = match hibernate_object_hash(ctx, table)? {
            Some(Value::Int(hash)) => hash,
            _ => 0,
        };
        let columns = match (columns, columns_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        let columns_hash = match hibernate_object_hash(ctx, columns)? {
            Some(Value::Int(hash)) => hash,
            _ => 0,
        };
        Ok(Some(Value::Int(
            31i32
                .wrapping_mul(31i32.wrapping_add(table_hash))
                .wrapping_add(columns_hash),
        )))
    })();
    if let Some(pin) = table_pin.or(columns_pin) {
        ctx.unpin_native_roots(pin);
    }
    result
}

fn native_hibernate_association_key_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(other)) => *other,
        _ => None,
    };
    let Some(other) = other else {
        return Ok(Some(Value::Int(0)));
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    if ctx.class_id_of_object(this) != ctx.class_id_of_object(other) {
        return Ok(Some(Value::Int(0)));
    }

    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| {
        let this = ctx.read_native_pin(this_pin, this);
        let other = ctx.read_native_pin(other_pin, other);
        let (table, columns) = hibernate_association_key_components(ctx, this);
        let (other_table, other_columns) = hibernate_association_key_components(ctx, other);
        let table_pin = table.map(|obj| ctx.pin_native_root(obj));
        let columns_pin = columns.map(|obj| ctx.pin_native_root(obj));
        let other_table_pin = other_table.map(|obj| ctx.pin_native_root(obj));
        let other_columns_pin = other_columns.map(|obj| ctx.pin_native_root(obj));

        let table = match (table, table_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        let other_table = match (other_table, other_table_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        if !hibernate_objects_equal(ctx, table, other_table)? {
            return Ok(Some(Value::Int(0)));
        }

        let columns = match (columns, columns_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        let other_columns = match (other_columns, other_columns_pin) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        Ok(Some(antlr_bool(hibernate_objects_equal(
            ctx,
            columns,
            other_columns,
        )?)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `NavigablePath` is the key type used while Hibernate de-duplicates the
/// circular EAGER-fetch graph.  Its value accessors are deliberately tiny
/// field reads, but the graph can call them at every level of a very deep
/// equality recursion.  Keep the Java `equals` implementation authoritative
/// and only bridge these allocation-free accessors.
fn native_hibernate_navigable_path_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(match ctx.get_field_by_name(this, "hashCode") {
        Value::Int(hash) => Value::Int(hash),
        _ => Value::Int(0),
    }))
}

fn native_hibernate_navigable_path_get_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, "alias")))
}

fn native_hibernate_navigable_path_get_real_parent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, "parent")))
}

fn native_hibernate_navigable_path_get_parent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let parent = match ctx.get_field_by_name(this, "parent") {
        Value::Object(Some(parent)) => parent,
        Value::Object(None) => return Ok(Some(Value::Object(None))),
        _ => return Ok(Some(Value::Object(None))),
    };

    // Mirrors `parent instanceof TreatedNavigablePath ? parent.getParent() :
    // parent`. Treated paths cannot themselves have treated parents (the Java
    // constructor asserts this), so reading that parent directly avoids a
    // recursive native dispatch while retaining the exact observable result.
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(parent))
        .as_deref()
        == Some("org/hibernate/spi/TreatedNavigablePath")
    {
        return Ok(Some(ctx.get_field_by_name(parent, "parent")));
    }
    Ok(Some(Value::Object(Some(parent))))
}

fn hibernate_navigable_path_is_entity_identifier(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(HIBERNATE_ENTITY_IDENTIFIER_NAVIGABLE_PATH)
}

fn hibernate_navigable_path_is_path(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
            .as_deref(),
        Some(
            HIBERNATE_NAVIGABLE_PATH
                | HIBERNATE_ENTITY_IDENTIFIER_NAVIGABLE_PATH
                | HIBERNATE_TREATED_NAVIGABLE_PATH
        )
    )
}

fn hibernate_pinned_objects_equal(
    ctx: &mut dyn NativeContext,
    left: Option<ObjectRef>,
    right: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(a), Some(b)) if a == b => Ok(true),
        (Some(a), Some(b)) => {
            let base = ctx.pin_native_root(a);
            let b_pin = ctx.pin_native_root(b);
            let result = (|| {
                let a = ctx.read_native_pin(base, a);
                let b = ctx.read_native_pin(b_pin, b);
                match ctx.invoke_virtual(
                    a,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(b))],
                )? {
                    Some(Value::Int(v)) => Ok(v != 0),
                    _ => Ok(false),
                }
            })();
            ctx.unpin_native_roots(base);
            result
        }
        _ => Ok(false),
    }
}

fn hibernate_navigable_path_field_object(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Option<ObjectRef> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(value) => value,
        _ => None,
    }
}

/// Exact native mirror of Hibernate 7.2's immutable `NavigablePath.equals`.
///
/// The all-EAGER RCA mappings repeatedly compare paths tens of frames deep
/// while constructing one loader graph. The Java body is correct but turns
/// each immutable field comparison into several interpreter frames. This
/// keeps the same identity, identifier-path, alias and parent rules while
/// retaining GC roots across the only re-entrant operations (String/parent
/// equality).
fn native_hibernate_navigable_path_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(other) = (match args.get(1) {
        Some(Value::Object(other)) => *other,
        _ => None,
    }) else {
        return Ok(Some(Value::Int(0)));
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }

    let other_is_path = hibernate_navigable_path_is_path(ctx, other);
    let other_is_role = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(other))
        .as_deref()
        == Some(HIBERNATE_NAVIGABLE_ROLE);
    if !other_is_path && !other_is_role {
        return Ok(Some(Value::Int(0)));
    }

    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| {
        let this = ctx.read_native_pin(this_pin, this);
        let other = ctx.read_native_pin(other_pin, other);
        let this_entity = hibernate_navigable_path_is_entity_identifier(ctx, this);
        let other_entity =
            other_is_path && hibernate_navigable_path_is_entity_identifier(ctx, other);

        let this_local = hibernate_navigable_path_field_object(ctx, this, "localName");
        let other_local = hibernate_navigable_path_field_object(ctx, other, "localName");
        let local_names_match = if hibernate_pinned_objects_equal(ctx, this_local, other_local)? {
            true
        } else if other_entity {
            if this_entity {
                false
            } else {
                let this = ctx.read_native_pin(this_pin, this);
                let other = ctx.read_native_pin(other_pin, other);
                let this_local = hibernate_navigable_path_field_object(ctx, this, "localName");
                let other_identifier =
                    hibernate_navigable_path_field_object(ctx, other, "identifierAttributeName");
                hibernate_pinned_objects_equal(ctx, this_local, other_identifier)?
            }
        } else if this_entity {
            let this = ctx.read_native_pin(this_pin, this);
            let other = ctx.read_native_pin(other_pin, other);
            let this_identifier =
                hibernate_navigable_path_field_object(ctx, this, "identifierAttributeName");
            let other_local = hibernate_navigable_path_field_object(ctx, other, "localName");
            hibernate_pinned_objects_equal(ctx, this_identifier, other_local)?
        } else {
            false
        };
        if !local_names_match {
            return Ok(Some(Value::Int(0)));
        }

        let this = ctx.read_native_pin(this_pin, this);
        let other = ctx.read_native_pin(other_pin, other);
        if other_is_path {
            let this_alias = hibernate_navigable_path_field_object(ctx, this, "alias");
            let other_alias = hibernate_navigable_path_field_object(ctx, other, "alias");
            let aliases_equal = hibernate_pinned_objects_equal(ctx, this_alias, other_alias)?;
            if !aliases_equal {
                return Ok(Some(Value::Int(0)));
            }
            let this = ctx.read_native_pin(this_pin, this);
            let other = ctx.read_native_pin(other_pin, other);
            let this_parent = hibernate_navigable_path_field_object(ctx, this, "parent");
            let other_parent = hibernate_navigable_path_field_object(ctx, other, "parent");
            hibernate_pinned_objects_equal(ctx, this_parent, other_parent)
        } else {
            let this_parent = hibernate_navigable_path_field_object(ctx, this, "parent");
            let other_parent = hibernate_navigable_path_field_object(ctx, other, "parent");
            hibernate_pinned_objects_equal(ctx, this_parent, other_parent)
        }
        .map(|equal| Some(Value::Int(equal as i32)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_hibernate_immutable_attribute_mapping_list_indexed_for_each(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    let this_pin = ctx.pin_native_root(this);
    let consumer_pin = ctx.pin_native_root(consumer);

    let result = (|| {
        let this = ctx.read_native_pin(this_pin, this);
        let list = match ctx.get_field_by_name(this, "list") {
            Value::Object(Some(list)) => list,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("ImmutableAttributeMappingList.list is null".to_string()),
                }
                .into())
            }
        };
        let list_pin = ctx.pin_native_root(list);
        let len = ctx.array_length(list);

        for i in 0..len {
            let list = ctx.read_native_pin(list_pin, list);
            let consumer = ctx.read_native_pin(consumer_pin, consumer);
            let element = ctx.get_array_element(list, i);
            let element_pin = match element {
                Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
                _ => None,
            };
            let element = match element_pin {
                Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
                None => element,
            };
            ctx.invoke_virtual(
                consumer,
                "accept",
                "(ILjava/lang/Object;)V",
                &[Value::Int(i as i32), element],
            )?;
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

fn hibernate_basic_valued_model_part_for_each_selectable(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    offset: i32,
    consumer: ObjectRef,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let consumer_pin = ctx.pin_native_root(consumer);
    let result = (|| {
        let this = ctx.read_native_pin(this_pin, this);
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        ctx.invoke_virtual(
            consumer,
            "accept",
            "(ILorg/hibernate/metamodel/mapping/SelectableMapping;)V",
            &[Value::Int(offset), Value::Object(Some(this))],
        )?;
        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

fn native_hibernate_basic_valued_model_part_for_each_selectable_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let offset = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "BasicValuedModelPart.forEachSelectable offset must be int".to_string(),
            }
            .into())
        }
    };
    let consumer = obj_arg(args, 2)?;
    hibernate_basic_valued_model_part_for_each_selectable(ctx, this, offset, consumer)
}

fn native_hibernate_basic_valued_model_part_for_each_selectable_zero(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = obj_arg(args, 1)?;
    hibernate_basic_valued_model_part_for_each_selectable(ctx, this, 0, consumer)
}

fn native_hibernate_annotation_usage_helper_find_usage(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let descriptor = obj_arg(args, 0)?;
    let map = obj_arg(args, 1)?;
    hibernate_find_annotation_usage(ctx, descriptor, map)
}

fn native_hibernate_annotation_usage_helper_get_usage_by_descriptor(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let descriptor = obj_arg(args, 0)?;
    let map = obj_arg(args, 1)?;
    let model_context = match args.get(2) {
        Some(Value::Object(model_context)) => *model_context,
        _ => None,
    };
    hibernate_get_usage_by_descriptor(ctx, descriptor, map, model_context)
}

fn native_hibernate_annotation_usage_helper_get_usage_by_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let annotation_type = obj_arg(args, 0)?;
    let map = obj_arg(args, 1)?;
    let model_context = obj_arg(args, 2)?;
    hibernate_get_usage_by_class(ctx, annotation_type, map, model_context)
}

fn native_hibernate_annotation_target_has_annotation_usage(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let annotation_type = obj_arg(args, 1)?;
    let model_context = obj_arg(args, 2)?;
    let this_pin = ctx.pin_native_root(this);
    let annotation_pin = ctx.pin_native_root(annotation_type);
    let model_context_pin = ctx.pin_native_root(model_context);

    let map = match hibernate_annotation_usage_map(ctx, this) {
        Ok(map) => map,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    let map_pin = ctx.pin_native_root(map);
    let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let contains_directly = match hibernate_map_contains_key(ctx, map, annotation_type)? {
        Some(Value::Int(v)) => v != 0,
        _ => false,
    };
    if contains_directly {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(1)));
    }

    let model_context = ctx.read_native_pin(model_context_pin, model_context);
    let registry = match ctx.invoke_virtual(
        model_context,
        "getAnnotationDescriptorRegistry",
        "()Lorg/hibernate/models/spi/AnnotationDescriptorRegistry;",
        &[],
    )? {
        Some(Value::Object(Some(registry))) => registry,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    let annotation_type = ctx.read_native_pin(annotation_pin, annotation_type);
    let descriptor = match ctx.invoke_virtual(
        registry,
        "getDescriptor",
        "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
        &[Value::Object(Some(annotation_type))],
    )? {
        Some(Value::Object(Some(descriptor))) => descriptor,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    let descriptor_pin = ctx.pin_native_root(descriptor);
    let repeatable = match ctx.invoke_virtual(descriptor, "isRepeatable", "()Z", &[])? {
        Some(Value::Int(v)) => v != 0,
        _ => false,
    };
    if !repeatable {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }

    let descriptor = ctx.read_native_pin(descriptor_pin, descriptor);
    let container = match ctx.invoke_virtual(
        descriptor,
        "getRepeatableContainer",
        "()Lorg/hibernate/models/spi/AnnotationDescriptor;",
        &[],
    )? {
        Some(Value::Object(Some(container))) => container,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    let container_type =
        match ctx.invoke_virtual(container, "getAnnotationType", "()Ljava/lang/Class;", &[])? {
            Some(Value::Object(Some(container_type))) => container_type,
            _ => {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Int(0)));
            }
        };
    let map = ctx.read_native_pin(map_pin, map);
    let result = hibernate_map_contains_key(ctx, map, container_type);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn register_hibernate_testing_util_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        HIBERNATE_TESTING_UTIL,
        "hasEffectiveAnnotation",
        "(Lorg/junit/jupiter/api/extension/ExtensionContext;Ljava/lang/Class;)Z",
        native_hibernate_testing_util_has_effective_annotation,
    );
}

fn native_hibernate_sqm_jpa_criteria_parameter_wrapper_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let hash = match args.first() {
        Some(Value::Object(Some(this))) => {
            match ctx.get_field_by_name(*this, "criteriaParameterId") {
                Value::Int(value) => value,
                _ => 0,
            }
        }
        _ => 0,
    };
    Ok(Some(Value::Int(hash)))
}

pub(crate) fn register_hibernate_models_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        "org/hibernate/query/sqm/tree/expression/SqmJpaCriteriaParameterWrapper",
        "hashCode",
        "()I",
        native_hibernate_sqm_jpa_criteria_parameter_wrapper_hash_code,
    );
    registry.register(
        HIBERNATE_ASSOCIATION_KEY,
        "hashCode",
        "()I",
        native_hibernate_association_key_hash_code,
    );
    registry.register(
        HIBERNATE_ASSOCIATION_KEY,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_hibernate_association_key_equals,
    );
    registry.register(
        HIBERNATE_NAVIGABLE_PATH,
        "hashCode",
        "()I",
        native_hibernate_navigable_path_hash_code,
    );
    registry.register(
        HIBERNATE_NAVIGABLE_PATH,
        "getAlias",
        "()Ljava/lang/String;",
        native_hibernate_navigable_path_get_alias,
    );
    registry.register(
        HIBERNATE_NAVIGABLE_PATH,
        "getRealParent",
        "()Lorg/hibernate/spi/NavigablePath;",
        native_hibernate_navigable_path_get_real_parent,
    );
    registry.register(
        HIBERNATE_NAVIGABLE_PATH,
        "getParent",
        "()Lorg/hibernate/spi/NavigablePath;",
        native_hibernate_navigable_path_get_parent,
    );
    registry.register(
        HIBERNATE_NAVIGABLE_PATH,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_hibernate_navigable_path_equals,
    );
    registry.register(
        HIBERNATE_IMMUTABLE_ATTRIBUTE_MAPPING_LIST,
        "indexedForEach",
        "(Lorg/hibernate/internal/util/IndexedConsumer;)V",
        native_hibernate_immutable_attribute_mapping_list_indexed_for_each,
    );
    registry.register(
        HIBERNATE_BASIC_VALUED_MODEL_PART,
        "forEachSelectable",
        "(ILorg/hibernate/metamodel/mapping/SelectableConsumer;)I",
        native_hibernate_basic_valued_model_part_for_each_selectable_offset,
    );
    registry.register(
        HIBERNATE_BASIC_VALUED_MODEL_PART,
        "forEachSelectable",
        "(Lorg/hibernate/metamodel/mapping/SelectableConsumer;)I",
        native_hibernate_basic_valued_model_part_for_each_selectable_zero,
    );
    registry.register(
        HIBERNATE_ANNOTATION_DESCRIPTOR_REGISTRY_STANDARD,
        "getDescriptor",
        "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
        native_hibernate_annotation_descriptor_registry_get_descriptor,
    );
    registry.register(
        "org/hibernate/models/spi/AnnotationDescriptorRegistry",
        "getDescriptor",
        "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
        native_hibernate_annotation_descriptor_registry_get_descriptor,
    );
    for owner in HIBERNATE_ANNOTATION_TARGET_OWNERS {
        registry.register(
            owner,
            "hasDirectAnnotationUsage",
            "(Ljava/lang/Class;)Z",
            native_hibernate_annotation_target_has_direct_annotation_usage,
        );
        registry.register(
            owner,
            "getDirectAnnotationUsage",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            native_hibernate_annotation_target_get_direct_annotation_usage,
        );
        registry.register(
            owner,
            "hasAnnotationUsage",
            "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Z",
            native_hibernate_annotation_target_has_annotation_usage,
        );
        registry.register(
            owner,
            "getAnnotationUsage",
            "(Lorg/hibernate/models/spi/AnnotationDescriptor;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
            native_hibernate_annotation_target_get_annotation_usage_by_descriptor,
        );
        registry.register(
            owner,
            "getAnnotationUsage",
            "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
            native_hibernate_annotation_target_get_annotation_usage_by_class,
        );
        registry.register(
            owner,
            "locateAnnotationUsage",
            "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
            native_hibernate_annotation_target_locate_annotation_usage,
        );
    }
    registry.register(
        HIBERNATE_ANNOTATION_USAGE_HELPER,
        "findUsage",
        "(Lorg/hibernate/models/spi/AnnotationDescriptor;Ljava/util/Map;)Ljava/lang/annotation/Annotation;",
        native_hibernate_annotation_usage_helper_find_usage,
    );
    registry.register(
        HIBERNATE_ANNOTATION_USAGE_HELPER,
        "getUsage",
        "(Lorg/hibernate/models/spi/AnnotationDescriptor;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
        native_hibernate_annotation_usage_helper_get_usage_by_descriptor,
    );
    registry.register(
        HIBERNATE_ANNOTATION_USAGE_HELPER,
        "getUsage",
        "(Ljava/lang/Class;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
        native_hibernate_annotation_usage_helper_get_usage_by_class,
    );
}

fn hibernate_uuid_from_parts(
    ctx: &mut dyn NativeContext,
    most: i64,
    least: i64,
) -> Result<Value, MethodCallFailed> {
    match ctx.new_object_initialized(
        "java/util/UUID",
        "(JJ)V",
        &[Value::Long(most), Value::Long(least)],
    )? {
        Some(Value::Object(Some(uuid))) => Ok(Value::Object(Some(uuid))),
        _ => Ok(Value::Object(None)),
    }
}

pub(crate) fn native_hibernate_uuid_v6_generate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    const UUID_EPOCH_OFFSET_100NS: i64 = 122_192_928_000_000_000;
    let strategy = match args.first() {
        Some(Value::Object(Some(strategy))) => *strategy,
        _ => return Ok(Some(Value::Object(None))),
    };
    let strategy_pin = ctx.pin_native_root(strategy);
    let result = (|| -> Result<Value, MethodCallFailed> {
        let strategy = ctx.read_native_pin(strategy_pin, strategy);
        let state_ref = match ctx.get_field_by_name(strategy, "lastState") {
            Value::Object(Some(state_ref)) => state_ref,
            _ => return Ok(Value::Object(None)),
        };
        let state_ref_pin = ctx.pin_native_root(state_ref);
        let result = (|| -> Result<Value, MethodCallFailed> {
            loop {
                let state_ref = ctx.read_native_pin(state_ref_pin, state_ref);
                let current = match ctx.get_field_volatile(state_ref, 0) {
                    Value::Object(Some(current)) => current,
                    _ => return Ok(Value::Object(None)),
                };
                let current_pin = ctx.pin_native_root(current);
                let current = ctx.read_native_pin(current_pin, current);
                let last_timestamp = ctx
                    .get_field_by_name(current, "lastTimestamp")
                    .as_long()
                    .unwrap_or(i64::MIN);
                let last_sequence = ctx
                    .get_field_by_name(current, "lastSequence")
                    .as_int()
                    .unwrap_or(i32::MIN);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let timestamp = now.as_secs() as i64 * 10_000_000
                    + now.subsec_nanos() as i64 / 100
                    + UUID_EPOCH_OFFSET_100NS;
                let (next_timestamp, next_sequence) = if last_timestamp < timestamp {
                    (timestamp, (sr_os_random_u64() & 0x3fff) as i32)
                } else if last_sequence == 0x3fff {
                    (last_timestamp + 1, (sr_os_random_u64() & 0x3fff) as i32)
                } else {
                    (last_timestamp, last_sequence + 1)
                };
                let next = match ctx.new_object_initialized(
                    "org/hibernate/id/uuid/UuidVersion6Strategy$State",
                    "(JI)V",
                    &[Value::Long(next_timestamp), Value::Int(next_sequence)],
                )? {
                    Some(Value::Object(Some(next))) => next,
                    _ => {
                        ctx.unpin_native_roots(current_pin);
                        return Ok(Value::Object(None));
                    }
                };
                let next_pin = ctx.pin_native_root(next);
                let state_ref_live = ctx.read_native_pin(state_ref_pin, state_ref);
                let current_live = ctx.read_native_pin(current_pin, current);
                let next_live = ctx.read_native_pin(next_pin, next);
                let committed = ctx.compare_and_swap_field(
                    state_ref_live,
                    0,
                    Value::Object(Some(current_live)),
                    Value::Object(Some(next_live)),
                );
                ctx.unpin_native_roots(next_pin);
                ctx.unpin_native_roots(current_pin);
                if committed {
                    let most = (next_timestamp << 4 & 0xffff_ffff_ffff_0000u64 as i64)
                        | 0x6000
                        | (next_timestamp & 0x0fff);
                    let least = (0x8000_0000_0000_0000u64 as i64)
                        | ((next_sequence as i64) << 48)
                        | ((sr_os_random_u64() as i64 & 0x0000_ffff_ffff_ffff)
                            | 0x0000_1000_0000_0000);
                    return hibernate_uuid_from_parts(ctx, most, least);
                }
            }
        })();
        ctx.unpin_native_roots(state_ref_pin);
        result
    })();
    ctx.unpin_native_roots(strategy_pin);
    result.map(Some)
}

pub(crate) fn native_hibernate_uuid_v7_generate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    const MAX_RANDOM_SEQUENCE: u64 = 0x3fff_ffff_ffff_ffff;
    let strategy = match args.first() {
        Some(Value::Object(Some(strategy))) => *strategy,
        _ => return Ok(Some(Value::Object(None))),
    };
    let strategy_pin = ctx.pin_native_root(strategy);
    let result = (|| -> Result<Value, MethodCallFailed> {
        let strategy = ctx.read_native_pin(strategy_pin, strategy);
        let state_ref = match ctx.get_field_by_name(strategy, "lastState") {
            Value::Object(Some(state_ref)) => state_ref,
            _ => return Ok(Value::Object(None)),
        };
        let state_ref_pin = ctx.pin_native_root(state_ref);
        let result = (|| -> Result<Value, MethodCallFailed> {
            loop {
                let state_ref = ctx.read_native_pin(state_ref_pin, state_ref);
                let current = match ctx.get_field_volatile(state_ref, 0) {
                    Value::Object(Some(current)) => current,
                    _ => return Ok(Value::Object(None)),
                };
                let current_pin = ctx.pin_native_root(current);
                let current = ctx.read_native_pin(current_pin, current);
                let previous_instant = match ctx.get_field_by_name(current, "lastTimestamp") {
                    Value::Object(Some(timestamp)) => timestamp,
                    _ => {
                        ctx.unpin_native_roots(current_pin);
                        return Ok(Value::Object(None));
                    }
                };
                let previous_seconds = ctx
                    .get_field_by_name(previous_instant, "seconds")
                    .as_long()
                    .unwrap_or(0);
                let previous_nanos = ctx
                    .get_field_by_name(previous_instant, "nanos")
                    .as_int()
                    .unwrap_or(0) as i64;
                let previous_sequence = ctx
                    .get_field_by_name(current, "lastSequence")
                    .as_long()
                    .unwrap_or(i64::MIN) as u64;
                let previous_sub_millis = ctx
                    .get_field_by_name(current, "nanos")
                    .as_long()
                    .unwrap_or(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let now_seconds = now.as_secs() as i64;
                let now_nanos = now.subsec_nanos() as i64;
                let now_millis = now_seconds * 1000 + now_nanos / 1_000_000;
                let previous_millis = previous_seconds * 1000 + previous_nanos / 1_000_000;
                let now_sub_millis = (now_nanos % 1_000_000) * 4096 / 1_000_000;
                let random_sequence = sr_os_random_u64() & MAX_RANDOM_SEQUENCE;
                let (next_seconds, next_nanos, next_sequence, next_sub_millis) = if previous_millis
                    < now_millis
                    || (previous_millis == now_millis && previous_sub_millis < now_sub_millis)
                {
                    (now_seconds, now_nanos, random_sequence, now_sub_millis)
                } else if previous_sequence >= random_sequence {
                    let adjusted = previous_nanos + 245;
                    let (seconds, nanos) = if adjusted >= 1_000_000_000 {
                        (previous_seconds + 1, adjusted - 1_000_000_000)
                    } else {
                        (previous_seconds, adjusted)
                    };
                    (
                        seconds,
                        nanos,
                        random_sequence,
                        (nanos % 1_000_000) * 4096 / 1_000_000,
                    )
                } else {
                    (
                        previous_seconds,
                        previous_nanos,
                        random_sequence,
                        previous_sub_millis,
                    )
                };
                let instant_class = ctx.ensure_class_initialized("java/time/Instant")?;
                let next_instant =
                    ctx.alloc_object(instant_class, ctx.class_num_total_fields(instant_class));
                let next_instant_pin = ctx.pin_native_root(next_instant);
                let next_instant = ctx.read_native_pin(next_instant_pin, next_instant);
                ctx.set_field_by_name(next_instant, "seconds", Value::Long(next_seconds));
                ctx.set_field_by_name(next_instant, "nanos", Value::Int(next_nanos as i32));
                let next = match ctx.new_object_initialized(
                    "org/hibernate/id/uuid/UuidVersion7Strategy$State",
                    "(Ljava/time/Instant;JJ)V",
                    &[
                        Value::Object(Some(next_instant)),
                        Value::Long(next_sequence as i64),
                        Value::Long(next_sub_millis),
                    ],
                )? {
                    Some(Value::Object(Some(next))) => next,
                    _ => {
                        ctx.unpin_native_roots(next_instant_pin);
                        ctx.unpin_native_roots(current_pin);
                        return Ok(Value::Object(None));
                    }
                };
                let next_pin = ctx.pin_native_root(next);
                let state_ref_live = ctx.read_native_pin(state_ref_pin, state_ref);
                let current_live = ctx.read_native_pin(current_pin, current);
                let next_live = ctx.read_native_pin(next_pin, next);
                let committed = ctx.compare_and_swap_field(
                    state_ref_live,
                    0,
                    Value::Object(Some(current_live)),
                    Value::Object(Some(next_live)),
                );
                ctx.unpin_native_roots(next_pin);
                ctx.unpin_native_roots(next_instant_pin);
                ctx.unpin_native_roots(current_pin);
                if committed {
                    let millis = next_seconds * 1000 + next_nanos / 1_000_000;
                    let most = (millis << 16 & 0xffff_ffff_ffff_0000u64 as i64)
                        | 0x7000
                        | (next_sub_millis & 0x0fff);
                    let least = 0x8000_0000_0000_0000u64 as i64 | next_sequence as i64;
                    return hibernate_uuid_from_parts(ctx, most, least);
                }
            }
        })();
        ctx.unpin_native_roots(state_ref_pin);
        result
    })();
    ctx.unpin_native_roots(strategy_pin);
    result.map(Some)
}
