#[allow(unused_imports)]
use super::*;

/// Class.isAnnotationPresent(Class) вЂ” searches superclass chain for @Inherited.
pub fn native_class_is_annotation_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => {
            return Err(annotation_class_null(
                ctx,
                this,
                AnnotationLookup::ClassIsPresent,
            ))
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_class_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Int(0))),
    };
    let target_desc = format!("L{};", ann_class_name);

    // Check this class
    let annotations = ctx.class_annotations(class_id);
    if annotations.iter().any(|a| a.type_descriptor == target_desc) {
        return Ok(Some(Value::Int(1)));
    }

    // Walk superclass chain for @Inherited annotations
    if is_inherited_annotation(ctx, &target_desc) {
        let mut current = ctx.superclass_of(class_id);
        while let Some(super_id) = current {
            let super_anns = ctx.class_annotations(super_id);
            if super_anns.iter().any(|a| a.type_descriptor == target_desc) {
                return Ok(Some(Value::Int(1)));
            }
            current = ctx.superclass_of(super_id);
        }
    }

    Ok(Some(Value::Int(0)))
}

/// Class.isAnnotation() вЂ” checks if the class itself is an annotation type
pub fn native_class_is_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    // ACC_ANNOTATION = 0x2000
    let flags = ctx.class_access_flags(class_id);
    Ok(Some(Value::Int(if (flags & 0x2000) != 0 { 1 } else { 0 })))
}

/// JDK `AnnotationSupport.getDirectlyAndIndirectlyPresent` for a single class's
/// annotation set: the directly-present `target` annotation (if any) PLUS the
/// repeated annotations carried inside its `@Repeatable` container, ordered by
/// declaration position. When the container is declared *before* the direct
/// annotation, the contained annotations come first (matching HotSpot's
/// `containerBeforeContainee` ordering).
///
/// Returns the matching `AnnotationData` clones, ready to be turned into proxies.
pub fn directly_and_indirectly_present(
    annotations: &[cratonvm_native_api::AnnotationData],
    target_desc: &str,
    container_desc: Option<&str>,
) -> Vec<cratonvm_native_api::AnnotationData> {
    use cratonvm_native_api::AnnotationElementValue as AEV;

    // Directly present: at most one per type in a well-formed class file.
    let direct_pos = annotations
        .iter()
        .position(|a| a.type_descriptor == target_desc);

    // Indirectly present: the container's `value()` array, filtered to `target`.
    let mut indirect: Vec<cratonvm_native_api::AnnotationData> = Vec::new();
    let mut container_pos: Option<usize> = None;
    if let Some(cdesc) = container_desc {
        if let Some(cpos) = annotations.iter().position(|a| a.type_descriptor == cdesc) {
            container_pos = Some(cpos);
            if let Some((_, AEV::Array(elems))) = annotations[cpos]
                .elements
                .iter()
                .find(|(name, _)| name == "value")
            {
                for elem in elems {
                    if let AEV::Annotation(nested) = elem {
                        if nested.type_descriptor == target_desc {
                            indirect.push(nested.clone());
                        }
                    }
                }
            }
        }
    }

    match direct_pos {
        None => indirect,
        Some(dpos) => {
            let direct_ann = annotations[dpos].clone();
            // Container declared before the direct annotation в‡’ contained first.
            let container_first = container_pos.map_or(false, |cpos| cpos < dpos);
            if container_first {
                indirect.push(direct_ann);
                indirect
            } else {
                let mut result = Vec::with_capacity(indirect.len() + 1);
                result.push(direct_ann);
                result.extend(indirect);
                result
            }
        }
    }
}

/// If the annotation type `ann_class_id` is `@Repeatable`, return the type
/// descriptor of its container annotation (read from `@Repeatable`'s `value()`).
pub fn repeatable_container_desc(ctx: &mut dyn NativeContext, ann_class_id: ClassId) -> Option<String> {
    use cratonvm_native_api::AnnotationElementValue as AEV;
    let ann_type_annotations = ctx.class_annotations(ann_class_id);
    let repeatable = ann_type_annotations
        .iter()
        .find(|a| a.type_descriptor == "Ljava/lang/annotation/Repeatable;")?;
    repeatable.elements.iter().find_map(|(name, v)| match v {
        AEV::Class(desc) if name == "value" => Some(desc.clone()),
        _ => None,
    })
}

/// Shared implementation of `Class.getAnnotationsByType` (`inherit = true`:
/// follow the `@Inherited` superclass chain when nothing is present locally)
/// and `Class.getDeclaredAnnotationsByType` (`inherit = false`).
///
/// Both merge the directly-present annotation with the contents of its
/// `@Repeatable` container вЂ” see [`directly_and_indirectly_present`]. The prior
/// implementation only unwrapped the container when there was NO direct match,
/// so `@Foo("A") @FooContainer({@Foo("B"),@Foo("C")})` returned just `[A]`
/// instead of `[A, B, C]`; and it never followed the `@Inherited` chain, so a
/// repeatable annotation declared on a superclass returned `[]`.
pub fn class_annotations_by_type_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    inherit: bool,
) -> MethodCallResult {
    // i7-L2: `Annotation[]`, not `Object[]`, on the edge returns too (the
    // populated path types the queried annotation; these have none to hand).
    let empty = |ctx: &mut dyn NativeContext| {
        let component = annotation_component_class_id(ctx);
        let arr = ctx.new_ref_array(component, 0);
        Ok(Some(Value::Object(Some(arr))))
    };
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return empty(ctx),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => return Err(annotation_class_null(ctx, this, AnnotationLookup::Direct)),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return empty(ctx),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_class_mirror) {
        Some(id) => id,
        None => return empty(ctx),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return empty(ctx),
    };
    let target_desc = format!("L{};", ann_class_name);
    let container_desc = repeatable_container_desc(ctx, ann_class_id);

    let this_annotations = ctx.class_annotations(class_id);
    let mut matching =
        directly_and_indirectly_present(&this_annotations, &target_desc, container_desc.as_deref());

    // `getAnnotationsByType` follows the @Inherited chain when (and only when)
    // nothing is directly/indirectly present on this class.
    if matching.is_empty() && inherit && is_inherited_annotation(ctx, &target_desc) {
        let mut current = ctx.superclass_of(class_id);
        while let Some(super_id) = current {
            let super_anns = ctx.class_annotations(super_id);
            matching = directly_and_indirectly_present(
                &super_anns,
                &target_desc,
                container_desc.as_deref(),
            );
            if !matching.is_empty() {
                break;            }
            current = ctx.superclass_of(super_id);
        }
    }

    // `getAnnotationsByType(Class<A>)` is declared to return `A[]`, so the array
    // component is the QUERIED annotation type — not `java/lang/Object`, which
    // made `Base.class.getAnnotationsByType(Tag.class).getClass().getName()`
    // report `[Ljava.lang.Object;` where HotSpot reports `[LTag;`.
    // GC-safe: `create_annotation_proxy_with_type` allocates (see
    // `build_mirror_array`). Resolve every type up front so an unresolvable one
    // is OMITTED here rather than published as a null-`annotationType()` proxy
    // — see `resolvable_annotations`.
    let resolvable = resolvable_annotations(ctx, Some(class_id), &matching);
    let container_loader = annotation_container_loader(ctx, class_id);
    // gen r4w3/rooting: each proxy build is a GC point and the closure reuses
    // the loader on the next element; pin it and re-read per element, as
    // `build_annotation_array_for` does.
    let container_loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    let arr = build_mirror_array_comp(ctx, ann_class_id, resolvable.len(), |ctx, i| {
        let loader_cur = match (container_loader, container_loader_pin) {
            (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
            _ => None,
        };
        let (ann, ann_cid) = resolvable[i];
        Ok(create_annotation_proxy_with_type(
            ctx,
            ann,
            ann_cid,
            Some(class_id),
            loader_cur,
        )?)
    })?;
    if let Some(pin) = container_loader_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `Class.getAnnotationsByType(Class)` вЂ” associated annotations: direct +
/// `@Repeatable` container contents, following the `@Inherited` superclass chain.
pub fn native_class_get_annotations_by_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_annotations_by_type_impl(ctx, args, true)
}

/// `Class.getDeclaredAnnotationsByType(Class)` вЂ” like `getAnnotationsByType` but
/// limited to THIS class's own annotations (no `@Inherited` superclass walk).
pub fn native_class_get_declared_annotations_by_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_annotations_by_type_impl(ctx, args, false)
}

/// `Method.getAnnotationsByType(Class)` / `getDeclaredAnnotationsByType` вЂ”
/// same direct-match + @Repeatable-container unwrap as
/// `native_class_get_annotations_by_type`, but over METHOD annotations.
/// Without this native the call falls through to the real
/// `Executable.getAnnotationsByType` bytecode, whose `declaredAnnotations()`
/// re-parses raw class-file annotation bytes via `AnnotationParser` вЂ” bytes
/// our synthetic Method objects don't carry вЂ” and dies with
/// `AnnotationFormatError: Unexpected end of annotations.` (JUnit's
/// `findRepeatableAnnotations` walks this for @ParameterizedTest argument
/// sources; Spring Boot buildSrc DependencyVersionUpgradeTests ran 0 of 63
/// invocations).
pub fn native_method_get_annotations_by_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // i7-L2: `Annotation[]`, not `Object[]`, on the edge returns too (the
    // populated path types the queried annotation; these have none to hand).
    let empty = |ctx: &mut dyn NativeContext| {
        let component = annotation_component_class_id(ctx);
        let arr = ctx.new_ref_array(component, 0);
        Ok(Some(Value::Object(Some(arr))))
    };
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return empty(ctx),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => {
            return Err(annotation_class_null(
                ctx,
                this,
                AnnotationLookup::ExecutableByType,
            ))
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => return empty(ctx),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_class_mirror) {
        Some(id) => id,
        None => return empty(ctx),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return empty(ctx),
    };
    let target_desc = format!("L{};", ann_class_name);
    let container_desc = repeatable_container_desc(ctx, ann_class_id);
    let annotations = ctx.method_annotations(class_id, &method_name, &method_desc);

    // Direct + @Repeatable container contents, in declaration order. Methods
    // have no `@Inherited` semantics, so there is no superclass walk. The prior
    // code only unwrapped the container when no direct match existed, dropping
    // the contained annotations whenever a direct one was also present.
    let matching =
        directly_and_indirectly_present(&annotations, &target_desc, container_desc.as_deref());

    // `A[]`, same as the Class-holder sibling above.
    // GC-safe: `create_annotation_proxy_with_type` allocates (see
    // `build_mirror_array`). Resolve every type up front so an unresolvable one
    // is OMITTED here rather than published as a null-`annotationType()` proxy
    // — see `resolvable_annotations`.
    let resolvable = resolvable_annotations(ctx, Some(class_id), &matching);
    let container_loader = annotation_container_loader(ctx, class_id);
    // gen r4w3/rooting: as in `class_annotations_by_type_impl` — pin the
    // loader across the per-element proxy builds and re-read it each time.
    let container_loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    let arr = build_mirror_array_comp(ctx, ann_class_id, resolvable.len(), |ctx, i| {
        let loader_cur = match (container_loader, container_loader_pin) {
            (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
            _ => None,
        };
        let (ann, ann_cid) = resolvable[i];
        Ok(create_annotation_proxy_with_type(
            ctx,
            ann,
            ann_cid,
            Some(class_id),
            loader_cur,
        )?)
    })?;
    if let Some(pin) = container_loader_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Helper: extract declaring class ID and field name from a Field reflection object.
pub fn field_class_and_name(
    ctx: &dyn NativeContext,
    field_obj: ObjectRef,
) -> Option<(ClassId, String)> {
    let mirror = match ctx.get_field_by_name(field_obj, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let class_id = mirror_class_id(ctx, mirror)?;
    let name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    Some((class_id, name))}

/// Helper: extract declaring class ID, method name, and descriptor from a Method reflection object.
pub fn method_class_name_desc(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
) -> Option<(ClassId, String, String)> {
    let mirror = match method_clazz_value(ctx, method_obj) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let class_id = mirror_class_id(ctx, mirror)?;
    let receiver_class = ctx.class_name_of_id(ctx.class_id_of_object(method_obj));
    let name_value = if receiver_class.as_deref() == Some("java/lang/reflect/Constructor") {
        ctx.get_field_by_name(method_obj, "name")
    } else {
        method_name_value(ctx, method_obj)
    };
    let name = match name_value {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => {
            // `java.lang.reflect.Constructor` has no `name` field (its
            // getName() returns the declaring-class name); the method it
            // describes is always `<init>`. Detect by the receiver's runtime
            // class so a genuinely malformed `Method` still returns `None`.
            // This lets the shared Method annotation natives
            // (getDeclaredAnnotations / getParameterAnnotations / вЂ¦), when
            // registered for Constructor too, resolve the `<init>` metadata вЂ”
            // otherwise constructor annotations fall through to real JDK
            // bytecode that reads raw `annotations`/`parameterAnnotations`
            // byte[] fields CratonVM never populates (Jackson "no Creators").
            if receiver_class.as_deref() == Some("java/lang/reflect/Constructor") {
                "<init>".to_string()
            } else {
                return None;
            }
        }
    };
    let mut desc = method_descriptor_for_invoke(ctx, method_obj);
    if desc.is_empty() {
        return None;
    }
    // Constructors always return void. The mirror-based descriptor composer
    // defaults a missing `returnType` to `Ljava/lang/Object;` (a Constructor
    // reflective object has no `returnType` field), and
    // `method_descriptor_for_invoke` then prefers that mismatched composed
    // descriptor вЂ” so the `<init>` lookup below would key on `(вЂ¦)Ljava/lang/
    // Object;` and never match the real `(вЂ¦)V` method metadata. Coerce the
    // return to `V` so annotation/param-annotation lookups for `<init>` hit.
    if name == "<init>" {
        if let Some(close) = desc.find(')') {
            desc.truncate(close + 1);
            desc.push('V');
        }
    }
    Some((class_id, name, desc))
}

/// Field.getAnnotations() / Field.getDeclaredAnnotations()
pub fn native_field_get_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let (class_id, field_name) = match field_class_and_name(ctx, this) {
        Some(v) => v,
        None => {
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let annotations = ctx.field_annotations(class_id, &field_name);
    let arr = build_annotation_array_for(ctx, Some(class_id), &annotations);
    Ok(Some(Value::Object(Some(arr?))))
}

/// Field.isAnnotationPresent(Class)
pub fn native_field_is_annotation_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ann_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (class_id, field_name) = match field_class_and_name(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Int(0))),
    };
    let target_desc = format!("L{};", ann_class_name);
    let annotations = ctx.field_annotations(class_id, &field_name);
    let present = annotations.iter().any(|a| a.type_descriptor == target_desc);
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

/// The JDK method an annotation lookup native serves, for the frames of its
/// null-argument raise ([`annotation_class_null`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AnnotationLookup {
    /// A lookup whose own body calls `Objects.requireNonNull`
    /// (`Class.getAnnotation` & co.): the one-hop rule lists it.
    Direct,
    /// `Class.isAnnotationPresent`.
    ClassIsPresent,
    /// `Method` / `Constructor` `.getAnnotation`.
    ExecutableGet,
    /// `Method` / `Constructor` `.isAnnotationPresent`.
    ExecutableIsPresent,
    /// `Method.getAnnotationsByType` / `getDeclaredAnnotationsByType` (one
    /// native): the declared form's chain, which the plain form's raise
    /// ignores (its served method is `Executable.getAnnotationsByType`).
    ExecutableByType,
}

/// A NULL annotation class handed to an annotation lookup: JDK 25 opens it
/// with `Objects.requireNonNull(annotationClass)`, or reaches one through
/// `getAnnotation`, so HotSpot throws a message-less `NullPointerException`
/// where `--compatible`'s bridges answered "not present" -- `null`, `false`
/// or an empty array, a genuine absence's answer (interpreter round i2 wave
/// 55, lane L3; probe `L3W55AnnotationNullArgs.java`;
/// `Field.getAnnotation`'s was fixed before, below). A raise HotSpot lists
/// two or more JDK frames below the served method notes that chain for the
/// funnel's mark (`NativeExceptionAccess::note_served_raise_chain`).
#[cold]
#[inline(never)]
pub fn annotation_class_null(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    lookup: AnnotationLookup,
) -> MethodCallFailed {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const PRESENT: &str = "(Ljava/lang/Class;)Z";
    const GET: &str = "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;";
    const BY_TYPE: &str = "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;";
    const REQUIRE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
    const ELEMENT: &str = "java/lang/reflect/AnnotatedElement";
    const ACCESSIBLE: &str = "java/lang/reflect/AccessibleObject";
    const EXECUTABLE: &str = "java/lang/reflect/Executable";
    let hop = |class: &'static str,
               name: &'static str,
               descriptor: &'static str,
               call: &'static str,
               call_desc: &'static str| {
        ServedRaiseHop {
            class,
            name,
            descriptor,
            site: ServedRaiseHopSite::LastCall {
                name: call,
                descriptor: Some(call_desc),
            },
        }
    };
    let require = ServedRaiseHop {
        class: "java/util/Objects",
        name: "requireNonNull",
        descriptor: REQUIRE,
        site: ServedRaiseHopSite::Raise,
    };
    // The executable's own class: `Method` or `Constructor`, whose
    // `getAnnotation` overrides `Executable`'s.
    let executable: &'static str = match ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .as_deref()
    {
        Some("java/lang/reflect/Constructor") => "java/lang/reflect/Constructor",
        _ => "java/lang/reflect/Method",
    };
    let hops = match lookup {
        AnnotationLookup::Direct => Vec::new(),
        AnnotationLookup::ClassIsPresent => vec![
            hop(
                "java/lang/Class",
                "isAnnotationPresent",
                PRESENT,
                "isAnnotationPresent",
                PRESENT,
            ),
            hop(
                ELEMENT,
                "isAnnotationPresent",
                PRESENT,
                "getAnnotation",
                GET,
            ),
            hop(
                "java/lang/Class",
                "getAnnotation",
                GET,
                "requireNonNull",
                REQUIRE,
            ),
            require,
        ],
        AnnotationLookup::ExecutableGet => vec![
            hop(executable, "getAnnotation", GET, "getAnnotation", GET),
            hop(EXECUTABLE, "getAnnotation", GET, "requireNonNull", REQUIRE),
            require,
        ],
        AnnotationLookup::ExecutableIsPresent => vec![
            hop(
                ACCESSIBLE,
                "isAnnotationPresent",
                PRESENT,
                "isAnnotationPresent",
                PRESENT,
            ),
            hop(
                ELEMENT,
                "isAnnotationPresent",
                PRESENT,
                "getAnnotation",
                GET,
            ),
            hop(executable, "getAnnotation", GET, "getAnnotation", GET),
            hop(EXECUTABLE, "getAnnotation", GET, "requireNonNull", REQUIRE),
            require,
        ],
        AnnotationLookup::ExecutableByType => vec![
            hop(
                ACCESSIBLE,
                "getDeclaredAnnotationsByType",
                BY_TYPE,
                "getAnnotationsByType",
                BY_TYPE,
            ),
            hop(
                EXECUTABLE,
                "getAnnotationsByType",
                BY_TYPE,
                "requireNonNull",
                REQUIRE,
            ),
            require,
        ],
    };
    if !hops.is_empty() {
        ctx.note_served_raise_chain(ServedRaiseChain {
            throwable_class: "java/lang/NullPointerException",
            hops,
        });
    }
    cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into()
}

/// Field.getAnnotation(Class) вЂ” returns a single annotation proxy or null.
pub fn native_field_get_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // A NULL annotation class is a NullPointerException, not "not present".
    //
    // MEASURED against HotSpot 25.0.3+9 in BOTH modes
    // (`probes/FieldMethodShadowSweep.java`): HotSpot NPE, this VM answered
    // null. `Field.getAnnotation` opens `Objects.requireNonNull(annotationClass)`
    // -- it is the first statement, ahead of any lookup.
    //
    // The direction matters here more than in most null cases: this method's
    // ORDINARY answer for an absent annotation is also null, so a null argument
    // and a genuine absence became indistinguishable. A caller passing a class
    // it failed to resolve reads "no such annotation" and takes the same branch
    // it would for a correct negative.
    let Some(Value::Object(Some(ann_mirror))) = args.get(1) else {
        // `requireNonNull(Object)`: no message (interpreter round i2 wave 53,
        // lane L3; probe `L3W53ReflectionNullArgs`); its frames are the
        // funnel's mark's.
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    };
    let ann_mirror = *ann_mirror;
    let (class_id, field_name) = match field_class_and_name(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    let target_desc = format!("L{};", ann_class_name);
    let annotations = ctx.field_annotations(class_id, &field_name);
    let container_loader = annotation_container_loader(ctx, class_id);
    // gen r4w3/rooting: an `Ok(None)` build can still have passed a GC point,
    // and the loader is reused on the next candidate; pin and re-read it.
    let container_loader_pin = container_loader.map(|loader| ctx.pin_native_root(loader));
    for ann in &annotations {
        if ann.type_descriptor == target_desc {
            let loader_cur = match (container_loader, container_loader_pin) {
                (Some(loader), Some(pin)) => Some(ctx.read_native_pin(pin, loader)),
                _ => None,
            };
            // W7-26 — `?`. Same species as the three `Class` sites above; this
            // one reaches the builder directly rather than through the cache,
            // but `create_annotation_proxy` has the identical
            // `Result<Option<_>, MethodCallFailed>` shape, so `Ok(None)` (an
            // annotation type that would not resolve) still falls through to
            // null and only the `Err` arm changes. Caller-visible: a
            // `Field.getAnnotation` that fails now throws instead of agreeing
            // with `Field.isAnnotationPresent` = true and answering null.
            let built = create_annotation_proxy(ctx, ann, Some(class_id), loader_cur);
            if !matches!(built, Ok(None)) {
                if let Some(pin) = container_loader_pin {
                    ctx.unpin_native_roots(pin);
                }
                return built.map(|proxy| proxy.map(|p| Value::Object(Some(p))));
            }
        }
    }
    if let Some(pin) = container_loader_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(None)))
}

/// Method.getAnnotations() / Method.getDeclaredAnnotations()
pub fn native_method_get_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let comp = annotation_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let annotations = ctx.method_annotations(class_id, &method_name, &method_desc);
    if crate::nbflags().ann_trace {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        if cn.contains("SpringBootApplication")
            || cn.contains("EnableAutoConfiguration")
            || cn.ends_with("/Import")
            || cn.contains("ImportHttpServices")
        {
            eprintln!(
                "[MGA] class_id={} {}.{}{} -> {} method-anns",
                class_id.as_u32(),
                cn,
                method_name,
                method_desc,
                annotations.len()
            );
            for a in &annotations {
                eprintln!("    {} elements={}", a.type_descriptor, a.elements.len());
                for (en, ev) in &a.elements {
                    eprintln!("      {} -> {:?}", en, ev);
                }
            }
        }
    }
    let arr =
        build_method_annotation_array(ctx, class_id, &method_name, &method_desc, &annotations);
    Ok(Some(Value::Object(Some(arr?))))
}

/// Method.isAnnotationPresent(Class)
pub fn native_method_is_annotation_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ann_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => {
            return Err(annotation_class_null(
                ctx,
                this,
                AnnotationLookup::ExecutableIsPresent,
            ))
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Int(0))),
    };
    let target_desc = format!("L{};", ann_class_name);
    let annotations = ctx.method_annotations(class_id, &method_name, &method_desc);
    let present = annotations.iter().any(|a| a.type_descriptor == target_desc);
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

/// Method.getAnnotation(Class)
pub fn native_method_get_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Wave 55, lane L3: `requireNonNull`, not "not present".
        _ => {
            return Err(annotation_class_null(
                ctx,
                this,
                AnnotationLookup::ExecutableGet,
            ))
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_id = match mirror_class_id(ctx, ann_mirror) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    let target_desc = format!("L{};", ann_class_name);
    let annotations = ctx.method_annotations(class_id, &method_name, &method_desc);
    if crate::nbflags().ann_trace {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        if cn.contains("SpringBootApplication")
            || cn.ends_with("/Import")
            || cn.contains("ImportHttpServices")
        {
            eprintln!(
                "[GMA] class_id={} {}.{}{} target={} -> {} method-anns",
                class_id.as_u32(),
                cn,
                method_name,
                method_desc,
                target_desc,
                annotations.len()
            );
            for a in &annotations {
                eprintln!("    {} elements={}", a.type_descriptor, a.elements.len());
                for (en, ev) in &a.elements {
                    eprintln!("      {} -> {:?}", en, ev);
                }
            }
        }
    }
    for ann in &annotations {
        if ann.type_descriptor == target_desc {
            // Cache first — see `cached_annotation_proxy_resolving`.
            let key = method_annotation_proxy_key(&method_name, &method_desc, &ann.type_descriptor);
            if let Some(cached) = peek_annotation_proxy_cache(ctx, class_id, &key) {
                return Ok(Some(Value::Object(Some(cached))));
            }
            if let Some(ann_cid) = resolve_annotation_type_near(ctx, ann, Some(class_id)) {
                let proxy = cached_method_annotation_proxy(
                    ctx,
                    class_id,
                    &method_name,
                    &method_desc,
                    ann,
                    ann_cid,
                );
                return Ok(Some(Value::Object(Some(proxy?))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Annotation.annotationType() вЂ” returns the Class mirror of the annotation type
pub fn native_annotation_annotation_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Field 1 is the Class mirror
    let mirror = ctx.get_field(this, ANN_PROXY_TYPE_MIRROR);
    Ok(Some(mirror))
}

/// Method.getParameterAnnotations() вЂ” returns Annotation[][] (one row per parameter).
pub fn native_method_get_parameter_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let comp = annotation_array_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let comp = annotation_array_component_class_id(ctx);
            let empty = ctx.new_ref_array(comp, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    // The outer array must be typed `Annotation[][]` (component = `Annotation[]`)
    // and each inner array `Annotation[]` (component = `Annotation`), so a
    // reflective `(Annotation[][]) getParameterAnnotations()` cast succeeds.
    let outer_comp = annotation_array_component_class_id(ctx);
    let inner_comp = annotation_component_class_id(ctx);
    let param_annotations = ctx.method_parameter_annotations(class_id, &method_name, &method_desc);
    let aligned_annotations =
        align_parameter_annotations(ctx, class_id, &method_name, &method_desc, param_annotations);
    // GC-safety (2026-07-16): `outer` used to be filled by a loop whose body
    // (`build_annotation_array_for`) allocates (each inner `Annotation[]` +
    // its proxies) *before* `set_array_element` ran -- a GC triggered inside
    // that call left the un-pinned `outer` `ObjectRef` stale, corrupting the
    // whole `Annotation[][]` result. Pin `outer` across the loop and re-read
    // the forwarded reference each iteration, matching `build_mirror_array`'s
    // established pattern. This is the "getParameterAnnotations" residual gap
    // flagged in
    // `jit-junit-discovery-reflection-corruption.md`.
    let mut outer = ctx.new_ref_array(outer_comp, aligned_annotations.len());
    let outer_pin = ctx.pin_native_root(outer);
    for i in 0..aligned_annotations.len() {
        let anns = aligned_annotations
            .get(i)
            .map(|a| build_annotation_array_for(ctx, Some(class_id), a))
            .unwrap_or_else(|| Ok(ctx.new_ref_array(inner_comp, 0)));
        outer = ctx.read_native_pin(outer_pin, outer);
        ctx.set_array_element(outer, i, Value::Object(Some(anns?)));
    }
    outer = ctx.read_native_pin(outer_pin, outer);
    ctx.unpin_native_roots(outer_pin);
    Ok(Some(Value::Object(Some(outer))))
}

/// Access flags for method-parameter metadata from `MethodParameters`.
pub const PARAMETER_MODIFIER_SYNTHETIC: u16 = 0x1000;
pub const PARAMETER_MODIFIER_MANDATED: u16 = 0x8000;

/// Class access flags used when deciding constructor synthetic-shift policy.
pub const CLASS_ACCESS_STATIC: u16 = 0x0008;
pub const CLASS_ACCESS_ENUM: u16 = 0x4000;

/// Normalize raw parameter-annotation rows to JVM `Executable.getParameterAnnotations()`.
///
/// HotSpot accepts class-files where constructor synthetic/mandated parameters are
/// absent from `RuntimeVisibleParameterAnnotations`, and shifts rows right so
/// descriptor-indexed slots align.
pub fn align_parameter_annotations(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    method_name: &str,
    method_desc: &str,
    param_annotations: Vec<Vec<AnnotationData>>,
) -> Vec<Vec<AnnotationData>> {
    let param_count = count_method_params(method_desc);
    let mut normalized = if param_annotations.len() > param_count {
        param_annotations.into_iter().take(param_count).collect()
    } else {
        param_annotations
    };

    if method_name != "<init>" || param_count == 0 {
        normalized.resize(param_count, Vec::new());
        return normalized;
    }

    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let class_flags = ctx.class_access_flags(class_id) as u16;
    let is_nested_class = class_name.contains('$');
    let is_static_nested = (class_flags & CLASS_ACCESS_STATIC) != 0;
    let is_enum = (class_flags & CLASS_ACCESS_ENUM) != 0;
    let has_enclosing_method = ctx.enclosing_method(class_id).is_some();

    let shift = if is_enum && normalized.len() + 2 == param_count {
        param_count.saturating_sub(normalized.len())
    } else if !is_enum
        && normalized.len() + 1 == param_count
        && !is_static_nested
        && is_nested_class
        && !has_enclosing_method
    {
        let param_meta = ctx.method_parameters(class_id, method_name, method_desc);
        if param_meta.first().is_some_and(|(_, flags)| {
            (*flags & (PARAMETER_MODIFIER_SYNTHETIC | PARAMETER_MODIFIER_MANDATED)) != 0
        }) {
            param_count.saturating_sub(normalized.len())
        } else {
            0
        }
    } else {
        0
    };

    if shift > 0 {
        let mut shifted = vec![Vec::new(); param_count];
        for (i, ann) in normalized.iter().enumerate() {
            shifted[i + shift] = ann.clone();
        }
        shifted
    } else {
        normalized.resize(param_count, Vec::new());
        normalized
    }
}

/// Count the number of parameters in a method descriptor.
pub fn count_method_params(desc: &str) -> usize {
    let params = match desc.strip_prefix('(') {
        Some(rest) => match rest.find(')') {
            Some(end) => &rest[..end],
            None => return 0,
        },
        None => return 0,
    };
    let mut count = 0;
    let bytes = params.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'L' => {
                count += 1;
                // skip to ';'
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                // array dimension вЂ” don't count, advance to element type
                i += 1;
            }
            _ => {
                // primitive
                count += 1;
                i += 1;
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Generics / Type reflection (Session 19)
// ---------------------------------------------------------------------------

/// Class.getTypeParameters() вЂ” returns TypeVariable[] from class signature.
pub fn native_class_get_type_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // H23-3 Sec3: HotSpot returns `[Ljava.lang.reflect.TypeVariable;`, not
    // `Object[]` -- `reflection_component_id` resolves the real component so
    // every return path (including the four empty ones) is typed the same as
    // the populated one.
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let sig_str = match ctx.class_signature(class_id) {
        Some(s) => s,
        None => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let class_sig = match crate::generics::parse_class_signature(&sig_str) {
        Some(s) => s,
        None => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // GC-safety (2026-07-16): `arr` is freshly-allocated and not yet
    // reachable from any Java-visible root; each `type_param_to_java` call
    // allocates a TypeVariable (+ bounds array + name string) and can
    // trigger a GC that relocates `arr` mid-loop. Pin it across the fill
    // loop and re-read the forwarded reference before every store, matching
    // the `build_mirror_array` contract. gc-common w30-b: `this` is pinned
    // BEFORE that allocation (a GC point), not after it; the oldest pin is
    // released once, after the last read.
    let this_pin = ctx.pin_native_root(this);
    let tv_cid = reflection_component_id(ctx, "java/lang/reflect/TypeVariable");
    let mut arr = ctx.new_ref_array(tv_cid, class_sig.type_params.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, tp) in class_sig.type_params.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        // `Class.getTypeParameters()` must return the SAME TypeVariable
        // objects across repeated calls, exactly like HotSpot's
        // `Class.getGenericInfo()` soft-reference cache. Building a fresh
        // synthetic TypeVariable on every call (the previous behavior here)
        // breaks any algorithm that stashes a type variable from one call
        // (e.g. as a `ParameterizedType`'s actual-type-argument, baked in by
        // value) and later compares/looks it up against the result of a
        // SUBSEQUENT `getTypeParameters()` call on the same class — the two
        // references are never identical (nor equal, since equals() is by
        // declaring-class+name and a later call's object still passes that
        // check, but a plain HashMap key lookup by the OLD reference against
        // a map keyed by the NEW one still requires hashCode/equals to run,
        // and repeated re-creation means the cache backing
        // `resolve_declared_type_variable` keeps getting overwritten mid-walk).
        // Concretely: Hibernate Validator's `TypeHelper.resolveTypes` walks a
        // class hierarchy, and at each `ParameterizedType` level calls
        // `erased.getTypeParameters()` fresh to compute its substitution map;
        // for a 3+ level generic hierarchy (e.g.
        // `AbstractInstantBasedTimeValidator` -> `HibernateConstraintValidator`
        // -> `ConstraintValidator`) sharing reused type-variable names across
        // levels, repeated non-identical objects for the same (class, name)
        // corrupt the map this algorithm chases through, producing a cyclic
        // `while (map.containsKey(x)) x = map.get(x)` walk that never
        // terminates (observed as an indefinite CPU-pegged hang building the
        // built-in `ConstraintHelper`, e.g. for classes deriving from
        // `AbstractInstantBasedTimeValidator`). Reuse the cached object when
        // one already exists for (this, tp.name); only build+cache a new one
        // on first request.
        // A cached entry that is still a PLACEHOLDER - an `Object`-bounded
        // stand-in published while an EARLIER parameter's bound referenced this
        // one (`<T extends Thing<S>, S extends Something>`) - is not the
        // finished parameter. Fall through to `type_param_to_java`, which
        // repairs that same object in place rather than allocating a new one.
        let cached = if crate::generics::is_placeholder_type_parameter(ctx, this, &tp.name) {
            None
        } else {
            crate::generics::cached_building_type_parameter(ctx, this, &tp.name)        };
        let tv = cached.map(Ok).unwrap_or_else(|| {
            crate::generics::type_param_to_java(ctx, tp, Value::Object(Some(this)))
        })?;
        arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, tv);
    }
    arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(arr))))
}

/// Class.getGenericSuperclass() вЂ” returns Type for the generic superclass.
pub fn native_class_get_generic_superclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // CP16-1: the class's Signature answer, once (`generic_reflect_memo_key`).
    let memo_key = mirror_class_id(ctx, this)
        .and_then(|id| generic_reflect_memo_key(&*ctx, id, "", "getGenericSuperclass"));
    if let Some(key) = &memo_key {
        if let Some(hit) = cached_reflect_single(ctx, key) {
            return Ok(Some(hit));
        }
    }
    // bytebuddy_probe (agent-bb3) вЂ” apply the SAME Object/interface/array
    // short-circuits as native_class_get_superclass. ByteBuddy's hierarchy
    // walk uses `getGenericSuperclass()` in addition to `getSuperclass()`;
    // without this guard the IllegalStateException cycle returns via the
    // generic path even when the plain path is now protected.
    //
    // bytebuddy_probe (agent-bb4) вЂ” read name via the STRICT helper so a
    // reverse-map alias of non-Object в†’ Object does NOT short-circuit
    // a non-Object class. See `mirror_class_name_strict` for rationale.
    let strict_name = mirror_class_name_strict(ctx, this).unwrap_or_default();
    let this_name = if !strict_name.is_empty() {
        strict_name.clone()
    } else {
        mirror_class_name(ctx, this).unwrap_or_default()
    };
    let dbg_bb = dbg_bb_enabled();
    if strict_name == "java/lang/Object" || strict_name == "java.lang.Object" {
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getGenericSuperclass({}) -> null [object-early-strict]",
                this_name
            );
        }
        return Ok(Some(Value::Object(None)));
    }
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getGenericSuperclass({}) -> null [no-class-id]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    // Second-line Object guard via ClassId.
    if let Some(obj_id) = ctx.class_id_by_name("java/lang/Object") {
        if class_id == obj_id {
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getGenericSuperclass({}) -> null [object-by-id]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(None)));
        }
    }
    // Interfaces and arrays both report null for getGenericSuperclass per JLS.
    if ctx.is_interface_class(class_id) {
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getGenericSuperclass({}) -> null [interface]",
                this_name
            );
        }
        return Ok(Some(Value::Object(None)));
    }
    // Synthetic lambda proxies (class id >= 0x8000_0000) are not in the class
    // store, so the `superclass_of` fallback below returns None в†’ a *null*
    // generic superclass. But a lambda's concrete runtime class extends Object,
    // and HotSpot's `getGenericSuperclass()` returns the raw `Object` superclass
    // for a class with no Signature attribute. `getSuperclass()` already special-
    // cases this (SB-14, see `native_class_get_superclass`); mirror that guard
    // here so the plain and generic super-class paths stay consistent and never
    // report a null superclass for a concrete (non-interface) class. Spring's
    // `ResolvableType` generic-hierarchy walk calls `clazz.getGenericSuperclass()`
    // on functional-interface / listener lambdas, and a null there cascades into
    // the bug-06 family-5 `getDeclaredMethod on null` (a resolved class going
    // null). bug-06 family 5.
    if ctx.lambda_functional_interface(class_id).is_some() {
        if let Some(obj_id) = ctx.class_id_by_name("java/lang/Object") {
            let mirror = ctx.get_class_mirror(obj_id);
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getGenericSuperclass({}) -> java/lang/Object [lambda]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }
    // If class has a Signature attribute, parse it for the generic superclass
    if let Some(sig_str) = ctx.class_signature(class_id) {
        if let Some(class_sig) = crate::generics::parse_class_signature(&sig_str) {
            // Type-variable uses in the superclass type refer to THIS class's
            // type parameters.
            // gen r4w4/rooting2: a pinned scope (the thread-local is not a
            // root, and the conversion below runs `loadClass`/Java), released
            // as soon as the conversion returns.
            let decl_mirror = ctx.get_class_mirror(class_id);
            let gscope = crate::generics::GenericDeclScope::new_pinned(
                ctx,
                Value::Object(Some(decl_mirror)),
            );
            // SB-02b-#3: real ParameterizedTypeImpl so a generic supertype like
            // `AbstractList<Map<String, List<X>>>` renders its type name correctly.
            let val = crate::generics::typesig_to_real_type(ctx, &class_sig.super_class);
            gscope.release(ctx);
            let val = val?;
            // If signature resolution succeeded, return it
            let val = match (val, &memo_key) {
                (Value::Object(Some(t)), Some(key)) => {
                    Value::Object(Some(remember_reflect_single(ctx, key, t)))
                }
                (val, _) => val,
            };
            if !matches!(val, Value::Object(None)) {
                if dbg_bb {
                    eprintln!(
                        "[bb-dbg] getGenericSuperclass({}) -> <signature>",
                        this_name
                    );
                }
                return Ok(Some(val));
            }
        }
    }
    // Fallback: return the raw superclass as a Class mirror
    if let Some(super_id) = ctx.superclass_of(class_id) {
        // Self-cycle guard вЂ” symmetric with native_class_get_superclass.
        if super_id == class_id {
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getGenericSuperclass({}) -> null [self-cycle]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(None)));
        }
        let mirror = ctx.get_class_mirror(super_id);
        if dbg_bb {
            let parent_name = ctx.class_name_of_id(super_id).unwrap_or_default();
            eprintln!(
                "[bb-dbg] getGenericSuperclass({}) -> {}",
                this_name, parent_name
            );
        }
        Ok(Some(Value::Object(Some(mirror))))
    } else {
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getGenericSuperclass({}) -> null [no-super]",
                this_name
            );
        }
        Ok(Some(Value::Object(None)))
    }
}

/// The `method_type_params_cache` row holding class `class_id`'s
/// `getGenericInterfaces()` answer (JIT round 16, lane coldpath). The empty
/// method name names no method (JVMS 4.2.2), so the row can collide with no
/// `Method.getTypeParameters()` row; it shares that table's rooting, moving
/// remap, unload and teardown ([`gc_scan_method_type_params_roots`] and its
/// siblings), all keyed by the class, which is the right owner: every class
/// the Signature names is visible from the class's own loader.
pub fn generic_interfaces_memo_key(ctx: &dyn NativeContext, class_id: ClassId) -> MethodTypeParamsKey {
    (
        ctx.vm_identity(),
        class_id,
        String::new(),
        "getGenericInterfaces".to_string(),
    )
}

/// CP16-1 (JIT round 16, lane coldpath): the row of the per-member generic
/// reflection memo (`CRATONVM_GENERIC_REFLECT_MEMO`, default ON) for `member`
/// of class `class_id`, or `None` when the memo is off. HotSpot answers these
/// from a repository cached per class or per member (`Class.genericInfo`, and
/// a `Method`/`Field` copy delegating to its root's), so the same `Type`
/// object comes back on every call and from every lookup of the member; the
/// natives rebuilt it per call (~30 us for `getGenericSuperclass`).
///
/// The `tag` keeps the rows apart from each other and from the
/// `Method.getTypeParameters()` rows, whose tag is the bare descriptor:
/// `"getGenericSuperclass"` (member `""`, a name nothing has),
/// `"<desc>#getGenericReturnType"`, `"<desc>#getGenericParameterTypes"` and
/// `"#getGenericType"` (a field, by name, as `field_signature` looks it up).
/// No descriptor contains `#`. Rooting, moving remap, unload and teardown are
/// the table's ([`gc_scan_method_type_params_roots`] and its siblings), owned
/// by the declaring class.
pub fn generic_reflect_memo_key(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    member: &str,
    tag: &str,
) -> Option<MethodTypeParamsKey> {
    if !crate::nbflags().generic_reflect_memo {
        return None;
    }
    Some((
        ctx.vm_identity(),
        class_id,
        member.to_string(),
        tag.to_string(),
    ))
}

/// The single `Type` a [`generic_reflect_memo_key`] row holds (element 0 of a
/// one-element row), or `None`. Allocates nothing.
pub fn cached_reflect_single(ctx: &mut dyn NativeContext, key: &MethodTypeParamsKey) -> Option<Value> {
    // Current: every moving collection re-addresses the row before any
    // mutator resumes ([`gc_update_method_type_params_refs`]).
    let row = method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .copied()?;
    Some(ctx.get_array_element(row, 0))
}

/// Remember `val` as `key`'s single `Type` and answer it, read current after
/// the row's allocation (a GC point, across which it is pinned). Nothing
/// allocates between the row's allocation and its insert, so neither is
/// unrooted across a GC point.
pub fn remember_reflect_single(
    ctx: &mut dyn NativeContext,
    key: &MethodTypeParamsKey,
    val: ObjectRef,
) -> ObjectRef {
    let pin = ctx.pin_native_root(val);
    let row = new_reflect_type_array(ctx, 1);
    let val = ctx.read_native_pin(pin, val);
    ctx.unpin_native_roots(pin);
    ctx.set_array_element(row, 0, Value::Object(Some(val)));
    method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), row);
    val
}

/// Remember the freshly built `arr` as `key`'s `Type[]` row and answer a copy
/// of it, as `native_method_get_type_parameters` does (the row owns the array;
/// a racing builder's row is simply replaced).
pub fn remember_reflect_array(
    ctx: &mut dyn NativeContext,
    key: &MethodTypeParamsKey,
    arr: ObjectRef,
) -> ObjectRef {
    method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), arr);
    copy_cached_reflect_array(ctx, key).unwrap_or(arr)
}

/// Class.getGenericInterfaces() вЂ” returns Type[] for generic interfaces.
pub fn native_class_get_generic_interfaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = empty_class_array(ctx);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // JIT round 16 (lane coldpath): the class's Signature answer, built once
    // and handed out as a copy -- HotSpot's per-class `genericInfo`, whose
    // `Type` elements are the same objects on every call. Rebuilding it cost
    // ~65 us per call (a loader walk per type argument), and
    // `HashMap.comparableClassFor` asks once per tree-bin insert and lookup:
    // that alone was the ~140x of `R16ColdpathHashMap`'s treeified phases.
    // `CRATONVM_GENERIC_INTERFACES_MEMO=0` rebuilds on every call.
    let memo_key = if crate::nbflags().generic_interfaces_memo {
        mirror_class_id(ctx, this).map(|id| generic_interfaces_memo_key(&*ctx, id))
    } else {
        None
    };
    if let Some(key) = &memo_key {
        // Allocates only on a hit, which returns: `this` is current below.
        if let Some(copy) = copy_cached_reflect_array(ctx, key) {
            return Ok(Some(Value::Object(Some(copy))));
        }
    }
    // bytebuddy_probe (agent-bb3) вЂ” Object short-circuit (empty Type[]).
    // bytebuddy_probe (agent-bb4) вЂ” strict-name first; see other helpers.
    let strict_name = mirror_class_name_strict(ctx, this).unwrap_or_default();
    let this_name = if !strict_name.is_empty() {
        strict_name.clone()
    } else {
        mirror_class_name(ctx, this).unwrap_or_default()
    };
    let dbg_bb = dbg_bb_enabled();
    if strict_name == "java/lang/Object" || strict_name == "java.lang.Object" {
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getGenericInterfaces({}) -> [] [object-early-strict]",
                this_name
            );
        }
        let arr = empty_class_array(ctx);
        return Ok(Some(Value::Object(Some(arr))));
    }
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = empty_class_array(ctx);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    if crate::nbflags().dbg_lambda_generic && this_name.contains("ApplicationContextInitializer") {
        eprintln!(
            "[LAMBDA-GENERIC] getGenericInterfaces ENTRY this_name={this_name} class_id={class_id:?}"
        );
    }
    // If class has a Signature attribute, parse it for generic interfaces
    if let Some(sig_str) = ctx.class_signature(class_id) {
        if let Some(class_sig) = crate::generics::parse_class_signature(&sig_str) {
            if !class_sig.interfaces.is_empty() {
                // GC-safety (2026-07-16): both `class_mirror` (re-consulted
                // every iteration via `GenericDeclScope`) and `arr` (filled
                // by the allocating `typesig_to_real_type` per interface)
                // are held in Rust locals across allocating calls without
                // being GC roots. Pin both and re-read the forwarded
                // reference before each use, matching `build_mirror_array`.
                let class_mirror = ctx.get_class_mirror(class_id);
                let class_mirror_pin = ctx.pin_native_root(class_mirror);
                let raw_interfaces = ctx.class_interfaces(class_id);
                let mut arr = new_reflect_type_array(ctx, class_sig.interfaces.len());
                let arr_pin = ctx.pin_native_root(arr);
                // A raw-interface stand-in (below) is not memoized: a later
                // call may resolve the argument once its class is loadable.
                let mut degraded = false;
                for (i, iface) in class_sig.interfaces.iter().enumerate() {
                    let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
                    // Type-variable uses in an interface type refer to THIS
                    // class's type parameters.
                    // gen r4w4/rooting2: pinned for the conversion, released
                    // right after it (above `arr_pin`, so LIFO holds).
                    let gscope = crate::generics::GenericDeclScope::new_pinned(
                        ctx,
                        Value::Object(Some(class_mirror)),
                    );
                    // SB-02b-#3: real ParameterizedTypeImpl for generic interfaces.
                    let val = crate::generics::typesig_to_real_type(ctx, iface);
                    gscope.release(ctx);
                    let val = val?;
                    // A malformed or not-yet-resolvable generic argument must
                    // not leave a null element in Type[]. Java reflection
                    // degrades to the matching raw direct interface in this
                    // situation; Hibernate Validator immediately dereferences
                    // every returned Type while discovering ConstraintValidator
                    // implementations.
                    let val = if matches!(val, Value::Object(None)) {
                        degraded = true;
                        raw_interfaces
                            .get(i)
                            .map(|&id| Value::Object(Some(ctx.get_class_mirror(id))))
                            .unwrap_or(Value::Object(None))
                    } else {
                        val
                    };
                    arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, val);
                }
                arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(class_mirror_pin);
                if let (Some(key), false) = (&memo_key, degraded) {
                    // The row owns the array (a racing builder's row is simply
                    // replaced); the caller gets a copy, as
                    // `native_method_get_type_parameters` does.
                    method_type_params_cache()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key.clone(), arr);
                    if let Some(copy) = copy_cached_reflect_array(ctx, key) {
                        return Ok(Some(Value::Object(Some(copy))));
                    }
                }
                if crate::nbflags().dbg_lambda_generic
                    && this_name.contains("ApplicationContextInitializer")
                {
                    eprintln!(
                        "[LAMBDA-GENERIC] getGenericInterfaces(this_name={this_name}) via class_sig.interfaces = {:?}",
                        class_sig.interfaces
                    );
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
    }
    // Synthetic lambda proxies aren't in the class store (so `class_interfaces`
    // is empty below), but a lambda's concrete class implements exactly its
    // functional (SAM) interface. Mirror `native_class_get_interfaces`'s
    // special case so `getGenericInterfaces()` вЂ” which must return the same
    // raw `[SAM]` per the JDK contract when there's no generic Signature вЂ”
    // doesn't silently fall through to an empty array. An empty array here
    // broke AspectJ's `execution(* Supplier+.get())` pointcut matching against
    // a lambda's own SAM-method Method object: AspectJ's reflection-based
    // world consults `getGenericInterfaces()` (not just `getInterfaces()`)
    // when resolving the declaring class's supertype closure, concluded the
    // lambda implemented no interfaces at all, and returned "never matches" вЂ”
    // silently skipping the advisor (Spring's `AnnotationAwareAspectJAutoProxyCreator`
    // then never proxies the lambda bean at all).
    if let Some(iface_name) = ctx.lambda_functional_interface(class_id) {
        if let Some(iface_id) =
            lambda_functional_interface_id_loader_aware(ctx, class_id, &iface_name)
        {
            // HotSpot parity: a `LambdaMetafactory`-spun implementation
            // class carries NO `Signature` attribute at all, so real HotSpot's
            // `getGenericInterfaces()` hands back the RAW functional-interface
            // `Class` for every lambda -- never a `ParameterizedType`, and
            // regardless of how concretely the call site's target type was
            // parameterized (verified against `jdk-25.0.3.9-hotspot`; see
            // `lambda-getgenericinterfaces-fabricates-parameterizedtype-FIXED.md`).
            //
            // A previous session reconstructed a concrete `ParameterizedType`
            // here out of the lambda's call-site instantiated descriptor, so
            // that `GenericTypeResolver.resolveTypeArgument` could find a type
            // argument for a lambda-typed `ApplicationContextInitializer`. That
            // premise was wrong: on real HotSpot the resolver succeeds through
            // `ResolvableType`'s TYPE-VARIABLE BOUND fallback -- the raw
            // interface `Class` still reports its own `getTypeParameters()`,
            // and a variable nothing binds resolves to its declared bound --
            // not through any `ParameterizedType`. Fabricating one is itself
            // observably wrong, and broke two Spring idioms that depend on a
            // lambda's generics being reported as UNRESOLVABLE:
            // `ApplicationConversionService.addBean` (which branches on
            // `hasUnresolvableGenerics()` to pick a type-aware adapter) and
            // `LambdaSafe.GenericTypeFilter` (which must NOT pre-filter a
            // lambda callback, so the deliberate erasure-driven
            // `ClassCastException` its javadoc exists to catch still happens).
            // Same marker rule as `getInterfaces()` above: HotSpot returns the raw
            // `Class` for each, plus `java.io.Serializable` when the call site set
            // FLAG_SERIALIZABLE, and each `altMetafactory` marker.
            let iface_ids = lambda_proxy_interface_ids(ctx, class_id, iface_id);
            let class_comp = class_component_id(ctx);
            let arr = build_mirror_array_comp(ctx, class_comp, iface_ids.len(), |ctx, i| {
                Ok(ctx.get_class_mirror(iface_ids[i]))
            })?;
            return Ok(Some(Value::Object(Some(arr))));
        }
    }

    // Fallback (no generic Signature, or it had no interfaces): per the JDK
    // contract, `getGenericInterfaces()` returns the RAW direct superinterfaces
    // (the same `Class[]` as `getInterfaces()`), NOT an empty array. Returning
    // empty broke any type-closure / hierarchy walk that uses
    // `getGenericInterfaces()` on a non-generic interface вЂ” e.g. Weld's
    // `HierarchyDiscovery` for `BeanManager extends BeanContainer`, which then
    // omitted `BeanContainer` from the closure and rejected every container
    // lifecycle observer with `WELD-000409` (HIB-CV-20).
    let iface_ids = ctx.class_interfaces(class_id);
    // GC-safe: `get_class_mirror` may allocate a mirror (see `build_mirror_array`).
    // Component type is `Class` (HotSpot returns `[Ljava/lang/Class;`), not Object.
    let class_comp = class_component_id(ctx);
    let arr = build_mirror_array_comp(ctx, class_comp, iface_ids.len(), |ctx, i| {
        Ok(ctx.get_class_mirror(iface_ids[i]))
    })?;
    Ok(Some(Value::Object(Some(arr))))
}

/// Method.getGenericParameterTypes() вЂ” returns Type[] from method signature.
pub fn native_method_get_generic_param_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = empty_class_array(ctx);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let arr = empty_class_array(ctx);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // CP16-1: the member's Signature answer, once (`generic_reflect_memo_key`).
    let memo_key = generic_reflect_memo_key(
        &*ctx,
        class_id,
        &method_name,
        &format!("{method_desc}#getGenericParameterTypes"),
    );
    if let Some(key) = &memo_key {
        // Allocates only on a hit, which returns: `this` is current below.
        if let Some(copy) = copy_cached_reflect_array(ctx, key) {
            return Ok(Some(Value::Object(Some(copy))));
        }
    }
    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &method_desc) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            // Declaration scope for type-variable uses: a generic method's own
            // type parameters (`<T> ... toArray(T[])`) declare to the METHOD;
            // otherwise the uses (`Iterator<E>`) refer to the declaring CLASS.
            // GC-safety (2026-07-16): `decl_obj` (when it's a freshly
            // fetched class mirror) and `arr` are both held across the
            // allocating `typesig_to_real_type` calls in this loop without
            // being GC roots. Pin both and re-read the forwarded reference
            // before each use, matching `build_mirror_array`.
            let decl_obj = if method_sig.type_params.is_empty() {
                ctx.get_class_mirror(class_id)
            } else {
                this
            };
            let decl_pin = ctx.pin_native_root(decl_obj);
            let mut arr = new_reflect_type_array(ctx, method_sig.param_types.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, pt) in method_sig.param_types.iter().enumerate() {
                let decl_obj = ctx.read_native_pin(decl_pin, decl_obj);
                // gen r4w4/rooting2: pinned for the conversion, released right
                // after it (above `arr_pin`, so LIFO holds).
                let gscope = crate::generics::GenericDeclScope::new_pinned(
                    ctx,
                    Value::Object(Some(decl_obj)),
                );
                // SB-02b-#3: real ParameterizedTypeImpl for parameterized parameter
                // types (the firing path for synthetic Method objects; real Method
                // objects already run the JDK reifier bytecode).
                let val = crate::generics::typesig_to_real_type(ctx, pt);
                gscope.release(ctx);
                let val = val?;
                arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, val);
            }
            arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(decl_pin);
            if let Some(key) = &memo_key {
                arr = remember_reflect_array(ctx, key, arr);
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // Fallback: return raw parameter types from the JDK `parameterTypes` field.
    let param_types = method_parameter_types_value(ctx, this);
    Ok(Some(param_types))
}

/// Method.getGenericReturnType() вЂ” returns Type from method signature.
pub fn native_method_get_generic_return_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    // CP16-1: the member's Signature answer, once (`generic_reflect_memo_key`).
    let memo_key = generic_reflect_memo_key(
        &*ctx,
        class_id,
        &method_name,
        &format!("{method_desc}#getGenericReturnType"),
    );
    if let Some(key) = &memo_key {
        if let Some(hit) = cached_reflect_single(ctx, key) {
            return Ok(Some(hit));
        }
    }
    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &method_desc) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            let decl = if method_sig.type_params.is_empty() {
                Value::Object(Some(ctx.get_class_mirror(class_id)))
            } else {
                Value::Object(Some(this))
            };
            // gen r4w4/rooting2: pinned scope, released after the conversion.
            let gscope = crate::generics::GenericDeclScope::new_pinned(ctx, decl);
            // SB-02b-#3: real ParameterizedTypeImpl for a parameterized return type.
            let val = crate::generics::typesig_to_real_type(ctx, &method_sig.return_type);
            gscope.release(ctx);
            return Ok(Some(match (val?, &memo_key) {
                (Value::Object(Some(t)), Some(key)) => {
                    Value::Object(Some(remember_reflect_single(ctx, key, t)))
                }
                (val, _) => val,
            }));
        }
    }
    // Fallback: return raw return type from the JDK `returnType` field.
    Ok(Some(method_return_type_value(ctx, this)))
}

/// Process-global cache for `Method`/`Constructor.getTypeParameters()`
/// arrays, keyed by (VM instance, declaring class, method name+descriptor)
/// -- NOT by the `this` mirror's own `ObjectRef`, since distinct reflective
/// Method objects for the same method must still resolve to the SAME
/// `TypeVariable` instances (see below).
///
/// HotSpot's `Executable.getTypeParameters()` is backed by a `genericInfo`
/// field memoized once per Method object, so repeated calls -- including
/// calls made *indirectly*, e.g. by something resolving a `TypeVariable`'s
/// `getGenericDeclaration().getTypeParameters()` back to itself -- return
/// identity-stable `TypeVariableImpl`s. Without this cache, every call here
/// built a brand-new synthetic `TypeVariable`, so a type variable handed out
/// by `getGenericReturnType()`/`getGenericParameterTypes()` (which resolves
/// its own uses via `resolve_declared_type_variable` re-invoking
/// `getTypeParameters()`, see `generics.rs`) was never identity-equal to the
/// "official" one this method itself returns. ByteBuddy's
/// `TypeVariableSource.AbstractBase.findVariable` walk (used when generating
/// a Mockito mock's overriding methods) filters `getTypeVariables()` and can
/// fail this resolution when identity/consistency assumptions elsewhere in
/// its pipeline are violated -- surfacing as `IllegalArgumentException:
/// Cannot resolve T from class ...$MockitoMock$...` for any interface with a
/// generic method (e.g. `jakarta.persistence.EntityManagerFactory.<T> T
/// unwrap(Class<T>)`).
///
/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// the `var_handle_roots` consumers): the value used to be the array's
/// identity hash, read back through `read_var_handle_root`. That registry is
/// keyed by identity hash too, so an object registered later with the same
/// hash (a `VarHandle`, a `TypeVariable`, any of its thirteen other users)
/// answered this cache: `getTypeParameters()` could return a `VarHandle`.
///
/// gc-common w30-b (`common-w29e-reflection-type-variable-caches-pin-their-class-loaders`):
/// the value is the array ITSELF, reported by the `type-variables` row of
/// `vm/src/memory/native_roots.rs` ([`gc_scan_method_type_params_roots`], with
/// the key's class as its owner) and re-addressed after every moving
/// collection ([`gc_update_method_type_params_refs`]). It was a strong JNI
/// global root, and its elements name the `Method`, so every class with a
/// generic method that was asked for its type parameters kept its loader for
/// the life of the VM. Under the cycle's loader-conditional licence the array
/// is now pinned to the declaring class's loader; a built-in-loader class's
/// row is rooted. Rows go when their class unloads
/// ([`forget_unloaded_method_type_params`]) and at teardown
/// ([`forget_vm_lang_class_tables`]).
///
/// The cached array is never handed out: each call answers a COPY
/// ([`copy_cached_method_type_params`]), as HotSpot's
/// `GenericDeclRepository.getTypeParameters()` answers `typeParams.clone()`.
/// It used to answer the cached array itself, so a caller that stored into
/// the array it was given rewrote every later caller's answer.
pub type MethodTypeParamsKey = (usize, ClassId, String, String);

pub fn method_type_params_cache() -> &'static Mutex<FxHashMap<MethodTypeParamsKey, ObjectRef>> {
    static C: OnceLock<Mutex<FxHashMap<MethodTypeParamsKey, ObjectRef>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// A fresh copy of the `TypeVariable[]` cached under `key`, or `None` (having
/// allocated nothing) when there is no row. The copy's allocation is a GC
/// point: the cached array is pinned across it and read back current. Its
/// elements are the cached `TypeVariable`s themselves, so identity holds
/// across calls.
pub fn copy_cached_method_type_params(
    ctx: &mut dyn NativeContext,
    key: &MethodTypeParamsKey,
) -> Option<ObjectRef> {
    copy_cached_reflect_array(ctx, key)
}

/// [`copy_cached_method_type_params`] for any row of the table, answering a
/// fresh copy of the row's array with the row's own runtime class (the
/// class's `getGenericInterfaces()` row, [`generic_interfaces_memo_key`], is
/// a `Type[]`).
pub fn copy_cached_reflect_array(
    ctx: &mut dyn NativeContext,
    key: &MethodTypeParamsKey,
) -> Option<ObjectRef> {
    // Current: every moving collection re-addresses the row before any
    // mutator resumes ([`gc_update_method_type_params_refs`]).
    let cached = method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .copied()?;
    let len = ctx.array_length(cached);
    // The copy has the cached array's own runtime class (`Type[]`,
    // `TypeVariable[]`): a reference array's header carries its component
    // class id, so this needs no lookup by name -- which answered the untyped
    // `ClassId(0)` (an `Object[]`, wrong for `instanceof Type[]` and for an
    // `ArrayStoreException`) whenever the component was not found.
    let component = ctx.class_id_of_object(cached);
    let pin = ctx.pin_native_root(cached);
    let copy = ctx.new_ref_array(component, len);
    let cached = ctx.read_native_pin(pin, cached);
    ctx.unpin_native_roots(pin);
    for i in 0..len {
        let element = ctx.get_array_element(cached, i);
        ctx.set_array_element(copy, i, element);
    }
    Some(copy)
}

/// Root scan for the `type-variables` row of `vm/src/memory/native_roots.rs`
/// (gc-common w30-b): hand each of VM `vm`'s cached `TypeVariable[]` arrays to
/// `visit` with its declaring class id as the owner; the caller roots it or,
/// under THIS VM's loader-conditional licence, pins it to that class's user
/// loader (`native_roots::defer_or_root`). The rows are copied out and the
/// table lock dropped before the first `visit`.
pub fn gc_scan_method_type_params_roots(vm: usize, visit: &mut dyn FnMut(Option<u32>, ObjectRef)) {
    let rows: Vec<(u32, ObjectRef)> = {
        let cache = method_type_params_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.is_empty() {
            return;
        }
        cache
            .iter()
            .filter(|((owner, _, _, _), _)| *owner == vm)
            .map(|((_, class_id, _, _), arr)| (class_id.as_u32(), *arr))
            .collect()
    };
    for (class_id, arr) in rows {
        visit(Some(class_id), arr);
    }
}

/// Post-move remap for [`gc_scan_method_type_params_roots`]'s rows: VM `vm`'s
/// arrays the collection moved. A dead array (its class unloaded) keeps its
/// stale address until [`forget_unloaded_method_type_params`] drops its row
/// in the same epilogue; nothing reads a row in between.
pub fn gc_update_method_type_params_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let mut cache = method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for ((owner, _, _, _), arr) in cache.iter_mut() {
        if *owner != vm {
            continue;
        }
        if let Some(&new_addr) = pointer_map.get(&(arr.as_ptr() as usize)) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: the collector's forwarding address for a live object.
            *arr = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
}

/// gc-common w30-b: drop VM `vm`'s `Method.getTypeParameters()` rows of the
/// classes the collection just UNLOADED (`lib.rs::forget_unloaded_proxy_classes`,
/// from `vm::memory::gc::unload_dead_class_metadata`'s unloading path). Their
/// arrays were pinned to the dead loader, not rooted, so this collection did
/// not mark them; a row left behind would name a reclaimed object, which the
/// next root scan -- with the class's loader pin gone -- would report as a
/// ROOT. Answers the rows dropped.
pub fn forget_unloaded_method_type_params(vm: usize, class_ids: &[u32]) -> usize {
    if class_ids.is_empty() {
        return 0;
    }
    let ids: rustc_hash::FxHashSet<u32> = class_ids.iter().copied().collect();
    let mut cache = method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let before = cache.len();
    cache.retain(|(owner, class_id, _, _), _| *owner != vm || !ids.contains(&class_id.as_u32()));
    before - cache.len()
}

/// Method.getTypeParameters() вЂ” returns TypeVariable[] from method signature.
pub fn native_method_get_type_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // H23-3 Sec3, same defect one level over: type every return path, not
    // just the populated one.
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let cache_key: MethodTypeParamsKey = (
        ctx.vm_identity(),
        class_id,
        method_name.clone(),
        method_desc.clone(),
    );
    // Allocates only on a hit, which returns: `this` is current below.
    if let Some(copy) = copy_cached_method_type_params(ctx, &cache_key) {
        return Ok(Some(Value::Object(Some(copy))));
    }
    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &method_desc) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            if !method_sig.type_params.is_empty() {
                // GC-safety (2026-07-16): same unrooted-`arr`-across-
                // allocating-fill pattern as `native_class_get_type_parameters`.
                // gc-common w30-b: `this` is pinned BEFORE the array's
                // allocation (a GC point); it used to be pinned after it, so a
                // moving collection there left the pin holding a stale
                // address. The oldest pin is released once, after the last
                // read of either.
                let this_pin = ctx.pin_native_root(this);
                let tv_cid = reflection_component_id(ctx, "java/lang/reflect/TypeVariable");
                let mut arr = ctx.new_ref_array(tv_cid, method_sig.type_params.len());
                let arr_pin = ctx.pin_native_root(arr);
                for (i, tp) in method_sig.type_params.iter().enumerate() {
                    let this = ctx.read_native_pin(this_pin, this);
                    // genericDeclaration = the declaring Method/Constructor (`this`).
                    let tv =
                        crate::generics::type_param_to_java(ctx, tp, Value::Object(Some(this)));
                    arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, tv?);
                }
                arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(this_pin);
                // gc-common w30-b: the row owns the array (a racing builder's
                // row is simply replaced); the caller gets a copy.
                method_type_params_cache()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cache_key.clone(), arr);
                let answer = copy_cached_method_type_params(ctx, &cache_key).unwrap_or(arr);
                return Ok(Some(Value::Object(Some(answer))));
            }
        }
    }
    let arr = empty_reflection_array(ctx, "java/lang/reflect/TypeVariable");
    Ok(Some(Value::Object(Some(arr))))
}

/// Field.getGenericType() вЂ” returns Type from field signature.
///
/// WP2.1 (FAIL-10): Parses the JVMS В§4.7.9 Signature attribute on the field
/// (when present, e.g. `Ljava/util/List<Ljava/lang/String;>;` for
/// `List<String> list`) into a `ParameterizedType` runtime object. When no
/// Signature attribute is present (non-generic field), falls back to the
/// raw `Class<?>` returned by `Field.getType()`.
pub fn native_field_get_generic_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (class_id, field_name) = match field_class_and_name(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    // CP16-1: the field's Signature answer, once (`generic_reflect_memo_key`).
    let memo_key = generic_reflect_memo_key(&*ctx, class_id, &field_name, "#getGenericType");
    if let Some(key) = &memo_key {
        if let Some(hit) = cached_reflect_single(ctx, key) {
            return Ok(Some(hit));
        }
    }
    if let Some(sig_str) = ctx.field_signature(class_id, &field_name) {
        if let Some(field_sig) = crate::generics::parse_field_signature(&sig_str) {
            // Type-variable uses in a field type refer to the declaring class.
            // gen r4w4/rooting2: pinned scope, released after the conversion.
            let decl_mirror = ctx.get_class_mirror(class_id);
            let gscope = crate::generics::GenericDeclScope::new_pinned(
                ctx,
                Value::Object(Some(decl_mirror)),
            );
            // SB-02b-#3: build a REAL `sun.reflectвЂ¦ParameterizedTypeImpl` (not the
            // bare-interface synthetic) so a nested generic field type like
            // `Map<String, List<String>>` renders its `getTypeName()`/`toString()`
            // identically to HotSpot instead of `java.lang.reflect.ParameterizedType@вЂ¦`.
            let val = crate::generics::typesig_to_real_type(ctx, &field_sig);
            gscope.release(ctx);
            return Ok(Some(match (val?, &memo_key) {
                (Value::Object(Some(t)), Some(key)) => {
                    Value::Object(Some(remember_reflect_single(ctx, key, t)))
                }
                (val, _) => val,
            }));
        }
    }
    // Fallback: when no Signature attribute, getGenericType() в‰Ў getType().
    // The `type` field is the Class<?> mirror at the JDK-native layout slot.
    // `()Ljava/lang/reflect/Type;` — read by resolved index so an unwritten
    // slot answers null rather than `Int(0)`.
    Ok(Some(crate::field_read::ref_field(ctx, this, "type")))
}

/// RecordComponent.getGenericType() вЂ” returns Type from the component's
/// Signature attribute.
///
/// A record component named `n` of type `T` shares its name and generic
/// signature with the synthesized `private final T n` field, so we parse
/// that field's JVMS В§4.7.9 Signature directly (mirrors
/// `native_field_get_generic_type`). The real-JDK `RecordComponent`
/// bytecode delegates to a `sun.reflect.generics` repository CratonVM
/// doesn't implement, so without this override `getGenericType()` collapsed
/// e.g. `List<TestSlice>` to the raw `List` Class вЂ” which made Jackson (and
/// any record introspector) lose the element type and deserialize a nested
/// record list into `List<LinkedHashMap>`. Falls back to the raw `type`
/// Class mirror when no Signature attribute is present.
pub fn native_record_component_get_generic_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(mirror)) => mirror_class_id(ctx, mirror),
        _ => None,
    };
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    if let (Some(class_id), Some(name)) = (class_id, name) {
        // Interpreter round i5 wave 101, lane L3: the component's OWN
        // Signature (as `getRecordComponents0` now fills `signature`), not
        // the same-named field's.
        let index = ctx
            .record_components(class_id)
            .iter()
            .position(|(n, _)| *n == name);
        let component_sig = index.and_then(|i| ctx.record_component_signature(class_id, i));
        if let Some(sig_str) = component_sig {
            if let Some(field_sig) = crate::generics::parse_field_signature(&sig_str) {
                // gen r4w4/rooting2: pinned scope, released after the conversion.
                let decl_mirror = ctx.get_class_mirror(class_id);
                let gscope = crate::generics::GenericDeclScope::new_pinned(
                    ctx,
                    Value::Object(Some(decl_mirror)),
                );
                // SB-02b-#3: real ParameterizedTypeImpl for nested record-component
                // generics (e.g. `Map<String, List<X>>`) вЂ” see field path above.
                let val = crate::generics::typesig_to_real_type(ctx, &field_sig);
                gscope.release(ctx);
                return Ok(Some(val?));
            }
        }
    }
    // Fallback: getGenericType() в‰Ў getType(). `()Ljava/lang/reflect/Type;` —
    // see `native_field_get_generic_type` above.
    Ok(Some(crate::field_read::ref_field(ctx, this, "type")))
}

// --- java.lang.reflect.Modifier ---

pub fn modifier_check(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
    mask: i32,
) -> MethodCallResult {
    let flags = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(if (flags & mask) != 0 { 1 } else { 0 })))}

pub fn native_modifier_is_public(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0001)
}

pub fn native_modifier_is_private(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0002)
}

pub fn native_modifier_is_protected(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0004)
}

pub fn native_modifier_is_static(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0008)
}

pub fn native_modifier_is_final(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0010)
}

pub fn native_modifier_is_abstract(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0400)
}

// ---------------------------------------------------------------------------

pub fn native_temp_print_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(&value) = args.first() {
        ctx.record_printed_value(value);
    }
    Ok(None)
}

pub fn native_temp_print_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(&value) = args.first() {
        ctx.record_printed_value(value);
    }
    Ok(None)
}

// ===========================================================================
// Modifier extras
// ===========================================================================

pub fn native_modifier_is_synchronized(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0020)
}

pub fn native_modifier_is_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0040)
}

pub fn native_modifier_is_transient(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0080)
}

pub fn native_modifier_is_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0100)
}

pub fn native_modifier_is_interface(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0200)
}

pub fn native_modifier_is_strict(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0800)
}

pub fn native_modifier_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let flags = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // J3 of `r15w4-jdkonly6-review-findings-CLOSED-20260930.md` (round 15 wave 5,
    // lane jdkonly7; `--synthetic-jdk` only): the JDK's word order
    // (`public protected private abstract static final transient volatile
    // synchronized native strictfp interface`) -- this listed `private`
    // before `protected` and `abstract` after `static` -- and a fresh
    // `String`, as `Modifier.toString`'s `StringJoiner` builds (the text was
    // interned into the never-pruned pool).
    let s = java_modifier_to_string(flags);
    Ok(Some(Value::Object(Some(ctx.create_string_uninterned(&s)))))
}

pub fn native_modifier_class_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // PUBLIC | PROTECTED | PRIVATE | ABSTRACT | STATIC | FINAL | STRICT
    Ok(Some(Value::Int(
        0x0001 | 0x0004 | 0x0002 | 0x0400 | 0x0008 | 0x0010 | 0x0800,
    )))
}

pub fn native_modifier_interface_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(
        0x0001 | 0x0004 | 0x0002 | 0x0400 | 0x0008 | 0x0800,
    )))
}

pub fn native_modifier_field_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(
        0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0010 | 0x0040 | 0x0080,
    )))
}

pub fn native_modifier_method_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(
        0x0001 | 0x0002 | 0x0004 | 0x0400 | 0x0008 | 0x0010 | 0x0020 | 0x0100 | 0x0800,
    )))
}

pub fn native_modifier_constructor_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0x0001 | 0x0002 | 0x0004)))
}

// ===========================================================================
// Class extras
// ===========================================================================

pub fn native_class_get_component_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            if crate::nbflags().dbg_component_type {
                eprintln!("[CT-DBG] getComponentType receiver=null");            }
            return Ok(Some(Value::Object(None)));
        }
    };
    // bytebuddy_probe (agent-bb4) вЂ” STRICT-NAME read. If the strict reader
    // gives us a non-array name (Object included), short-circuit null
    // without consulting the reverse-map (which may alias to an array
    // type and incorrectly return a component class for Object).
    let name = {
        let strict = mirror_class_name_strict(ctx, this).unwrap_or_default();
        if !strict.is_empty() {
            strict
        } else {
            mirror_class_name(ctx, this).unwrap_or_default()
        }
    };
    // Strict mode: a real array class knows its own component (`array_info`).
    // Re-deriving it from the array's NAME resolves `p/X` in the global
    // namespace, so a child loader's `X[]` reported the application's `X` as
    // its component type.
    // `--compatible` (interpreter round i1 wave 48, lane L5): the same for a
    // component a USER loader defined -- `getClass()` of such an array is the
    // class manager's array class in both modes now
    // (`strict_array_class_id`), and the by-name route below answered the
    // application's same-named class (probe `L5W48UserArrayClassIdentity`
    // `component`). A built-in loader's component keeps that mode's route,
    // unless the `ComponentTypeFromArrayClass` row is on (interpreter round
    // i1 wave 50, lane L5): that route is `ensure_class_initialized` of the
    // component's NAME, so `Class.forName("[Lp.C;").getComponentType()` ran
    // `C.<clinit>` (JLS 12.4.1: nothing here initializes; probe
    // `L5W44LoaderReview`, `forname-array-noinit`).
    if name.starts_with("[L") || name.starts_with("[[") {
        if let Some(array_cid) = mirror_class_id(ctx, this) {
            if let Some(component_cid) = ctx.array_component_class_id(array_cid) {
                if ctx.aligned(cratonvm_types::HotSpotAlignment::ComponentTypeFromArrayClass) {
                    // The by-name route would have initialized a built-in
                    // loader's component (wave 51, lane L5: the alignment
                    // census). An initialized component is the common case
                    // and is decided by the first test.
                    if !ctx.is_jdk_only()
                        && !ctx.is_class_initialized(component_cid)
                        && ctx.loader_id_of_class(component_cid) < 3
                    {
                        ctx.note_alignment_divergence(
                            cratonvm_types::HotSpotAlignment::ComponentTypeFromArrayClass,
                        );
                    }
                    return Ok(Some(Value::Object(Some(
                        ctx.get_class_mirror(component_cid),
                    ))));
                }
                if ctx.loader_id_of_class(component_cid) >= 3 {
                    return Ok(Some(Value::Object(Some(
                        ctx.get_class_mirror(component_cid),
                    ))));
                }
            }
        }
    }
    // Array classes have names like "[I", "[Ljava/lang/String;"
    if let Some(component) = name.strip_prefix('[') {
        let comp_name = match component {
            "I" => "int",
            "J" => "long",
            "F" => "float",
            "D" => "double",
            "Z" => "boolean",
            "B" => "byte",
            "C" => "char",
            "S" => "short",
            _ if component.starts_with('L') && component.ends_with(';') => {
                &component[1..component.len() - 1]
            }
            _ => component,
        };
        // Primitive types don't have loadable classes вЂ” use primitive_class_mirror
        match comp_name {
            "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short"
            | "void" => {
                let mirror = ctx.primitive_class_mirror(comp_name);
                return Ok(Some(Value::Object(Some(mirror))));
            }
            _ if lambda_component_class_id(ctx, comp_name).is_some() => {
                // An array of a lambda proxy (`Array.newInstance(lambdaClass, n)`): the proxy
                // is not in the class store, so no by-name route can find it, but its name
                // carries its id (`<host>$$Lambda/0x<id>`, `lambda_proxy_class_name`).
                let cid = lambda_component_class_id(ctx, comp_name).unwrap_or(ClassId::new(0));
                return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
            }
            _ => {
                // A descriptor-backed array mirror produced for reflective
                // metadata retains its declaring loader in Class.classLoader.
                // Resolve its component through that exact namespace before
                // consulting the global array/class registry; otherwise
                // Method.getReturnType() on a fork-defined T[] leaks the
                // application-loader T and breaks repeatable-annotation and
                // generic reflection identity checks.
                if let Value::Object(Some(loader)) = ctx.get_field_by_name(this, "classLoader") {
                    if let Some(loader_id) =
                        crate::classloader::peek_loader_namespace_id(ctx, loader)
                    {
                        if comp_name.starts_with('[') {
                            let mirror = synthetic_class_mirror_for_loader(ctx, comp_name, loader);
                            return Ok(Some(Value::Object(Some(mirror))));
                        }
                        if let Some(cid) =
                            ctx.class_id_defined_by_loader_exact(comp_name, loader_id)
                        {
                            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                        }
                    }
                }
                if let Ok(cid) = ctx.ensure_class_initialized(comp_name) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
                // SB3.2 вЂ” when the component class can't be loaded
                // (e.g. nested array `[[L...;`, or absent class), still
                // return a non-null Class mirror so callers that assume
                // `array.componentType() != null` (Spring's
                // `ConstructorResolver.resolveAutowiredArgument` does
                // `Array.newInstance(type.componentType(), 0)`) do not NPE.
                return Ok(Some(Value::Object(Some(synthetic_class_mirror(
                    ctx, comp_name,
                )))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

/// The throwable `Class.arrayType()` raises for a component with no array
/// type: what the JDK body builds, `new UnsupportedOperationException(iae)`
/// around the `IllegalArgumentException` (no message) that
/// `Array.newInstance` threw -- so the message is the cause's `toString()`,
/// `java.lang.IllegalArgumentException`. Falls back to the message-only form
/// if the constructors cannot be run.
#[cold]
pub fn array_type_unsupported(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    const MESSAGE: &str = "java.lang.IllegalArgumentException";
    let iae = match ctx.new_object_initialized("java/lang/IllegalArgumentException", "()V", &[]) {
        Ok(Some(Value::Object(Some(iae)))) => Some(iae),
        _ => None,
    };
    if let Some(iae) = iae {
        // The cause stays pinned across the message and the outer object's
        // allocation; it is attached with `initCause` afterwards (the
        // `(String)` constructor leaves the cause unset), the idiom
        // `classloader_real::no_class_def_found_error` uses.
        let iae_pin = ctx.pin_native_root(iae);
        let msg = ctx.create_string(MESSAGE);
        let uoe = cratonvm_native_api::throwable_builder::construct_new(
            ctx,
            "java/lang/UnsupportedOperationException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(msg))],
        );
        if let Ok(Some(uoe)) = uoe {
            let uoe_pin = ctx.pin_native_root(uoe);
            let iae = ctx.read_native_pin(iae_pin, iae);
            let _ = ctx.invoke_virtual(
                uoe,
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                &[Value::Object(Some(iae))],
            );
            let uoe = ctx.read_native_pin(uoe_pin, uoe);
            ctx.unpin_native_roots(iae_pin);
            return MethodCallFailed::ExceptionThrown(uoe);
        }
        ctx.unpin_native_roots(iae_pin);
    }
    cratonvm_types::error::RuntimeError::UnsupportedOperationException {
        message: MESSAGE.to_string(),
    }
    .into()
}

/// `Class.arrayType()` вЂ” return a `Class` mirror that represents the
/// array type whose component is `this`.
///
/// JDK's bytecode implementation is `Array.newInstance(this, 0).getClass()`,
/// which on CratonVM has been observed to surface null entries in
/// Spring's `GenericConversionService$Converters.getClassHierarchy`
/// (which calls `Class.arrayType()` on the superclass / interfaces of an
/// array-typed argument and then dereferences the result with
/// `componentType()` on the next iteration). Bypass that fragile two-step
/// chain by synthesising the array-type mirror directly from the
/// component class's name, mirroring what `Object.getClass()` does for an
/// actual array object (see `native_object_get_class`).
pub fn native_class_array_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // A lambda proxy names its array as `getClass()` of one does (`[L<host>$$Lambda/0x<id>;`),
    // so the two answers are the same class (SBR5 walker).
    let name = mirror_class_id(ctx, this)
        .and_then(|cid| register_lambda_array_component(ctx, cid))
        .unwrap_or_else(|| mirror_class_name(ctx, this).unwrap_or_default());
    // The JDK body is `Array.newInstance(this, 0).getClass()` with its
    // `IllegalArgumentException` rethrown as `new
    // UnsupportedOperationException(iae)`: `void` has no array type, and
    // neither has an array class of 255 dimensions. This used to answer null
    // for `void` and synthesize a 256-dimension class (probe
    // `L4W29ReflectNewArray.java`, row 36).
    if crate::lang_system::reflect_new_array_refuses_component(&name, 1) {
        return Err(array_type_unsupported(ctx));
    }
    // Primitive component в†’ "[I", "[J", вЂ¦ ; object/array component в†’ "[L<name>;" or "[<arrayname>".
    let array_name = match name.as_str() {
        "int" => "[I".to_string(),
        "long" => "[J".to_string(),
        "float" => "[F".to_string(),
        "double" => "[D".to_string(),
        "boolean" => "[Z".to_string(),
        "byte" => "[B".to_string(),
        "char" => "[C".to_string(),
        "short" => "[S".to_string(),
        other if other.starts_with('[') => format!("[{other}"),
        other => format!("[L{};", other),
    };
    // Return the CANONICAL array-class mirror вЂ” the same `ClassId`-backed
    // mirror that the `T[].class` constant (`ldc`), `anewarray`, and an array
    // object's `getClass()` all resolve to. The old path went straight to
    // `primitive_class_mirror`, which mints a SEPARATE, non-interned mirror;
    // that broke identity (`Object.class.arrayType() != Object[].class`) and
    // in turn Spring's `GenericConversionService` converter registry, whose
    // `ConvertiblePair` keys compare `Class` by identity (e.g. a `String ->
    // String[]` conversion silently reported "no converter found"). Mirror
    // exactly what `Object.getClass()` does for a live array (see
    // `native_object_get_class`): resolve the array `ClassId` by name,
    // loading it on demand, and only fall back to the synthetic mirror for
    // degenerate names that fail to synthesise.
    if let Some(component_id) = mirror_class_id(ctx, this) {
        if let Some(arr_cid) = strict_array_class_id(ctx, component_id, &array_name) {
            let mirror = ctx.get_class_mirror(arr_cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }
    if let Some(arr_cid) = ctx.class_id_by_name(&array_name) {
        let mirror = ctx.get_class_mirror(arr_cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    let _ = ctx.load_class(&array_name);
    if let Some(arr_cid) = ctx.class_id_by_name(&array_name) {
        let mirror = ctx.get_class_mirror(arr_cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    let mirror = ctx.primitive_class_mirror(&array_name);
    Ok(Some(Value::Object(Some(mirror))))
}

pub fn native_class_get_package_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // A PRIMITIVE's package is `java.lang`, not the empty string.
    //
    // MEASURED against HotSpot 25.0.3+9 in BOTH modes
    // (`probes/ClassShadowSweep.java`), all six plus `void`:
    //
    //   int.class.getPackageName()   HotSpot "java.lang"   CratonVM ""
    //
    // The JDK's body is `if (isPrimitive()) return "java.lang";` before it
    // looks at anything else, and the reason is not arbitrary: a primitive's
    // wrapper, its `Class` mirror and `void` all live in `java.lang`, so code
    // that groups reflected types by package — a scanner, a doc generator, an
    // access check keyed on package — puts primitives with the classes they
    // belong to instead of into an anonymous default-package bucket.
    //
    // Checked BEFORE the cache, since the cache is keyed on `ClassId` and a
    // primitive mirror has one; a wrong answer would otherwise be memoised for
    // the life of the VM.
    //
    // Every answer below is INTERNED (round 14 wave 3, lane compat3): the JDK
    // body is `cn.substring(0, dot).intern()` (`Class.java:1156`), and since
    // `create_string` became pool-hit-or-fresh a computed name would be a
    // fresh String per call (`r14w3-compat3-lang-class-package-name-interning-patch`).
    if mirror_is_primitive(ctx, this) {
        return Ok(Some(Value::Object(Some(ctx.intern_string("java.lang")))));
    }
    // Cache the dotted package prefix per `ClassId` вЂ” invariant for the
    // program's lifetime. Cache-eligible mirrors are those owned by the
    // VM reverse map (rules out test-fixture ClassId(0) collisions).
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        if let Some(arc) = cache_get(&PACKAGE_NAME_CACHE, ctx.vm_identity(), class_id) {
            return Ok(Some(Value::Object(Some(ctx.intern_string(&arc)))));
        }
        // Synthetic lambda proxies aren't in the class store, so
        // `class_name_of_id` below misses them and this whole branch used to
        // fall through to the empty-string fallback further down (which reads
        // via the same class-store-backed `mirror_class_name` and gets nothing
        // either). Derive the package from the lambda's HOST class instead вЂ”
        // matches HotSpot, whose hidden lambda class lives in the host's
        // package. See `lambda_proxy_class_name` for the analogous `getName()`
        // special case.
        if let Some(host) = ctx.lambda_proxy_host(class_id) {
            let pkg = match host.rfind('/') {
                Some(pos) => host[..pos].replace('/', "."),
                None => String::new(),
            };
            return Ok(Some(Value::Object(Some(ctx.intern_string(&pkg)))));
        }
        if let Some(name) = ctx.class_name_of_id(class_id) {
            let pkg = package_name_of(ctx.vm_identity(), class_id, &name);
            return Ok(Some(Value::Object(Some(ctx.intern_string(&pkg)))));
        }
    }
    // Fallback (test fixtures, primitives) вЂ” original per-call derivation.
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let pkg = if name.starts_with('[') {
        array_descriptor_to_package_name(&name).unwrap_or_default()
    } else if let Some(pos) = name.rfind('/') {
        name[..pos].replace('/', ".")
    } else if let Some(pos) = name.rfind('.') {
        name[..pos].to_string()
    } else {
        String::new()
    };
    Ok(Some(Value::Object(Some(ctx.intern_string(&pkg)))))
}

// ---------------------------------------------------------------------------
// T19_H10_GET_PACKAGE: real `Class.getPackage()` вЂ” was `native_return_null`,
// which broke any code path that did
// `Foo.class.getPackage().getImplementationVersion()` (a popular way to
// discover the version of a library at runtime вЂ” Hibernate, Logback, JBoss
// LogManager all do this in `<clinit>` and would previously NPE on the
// `.getImplementationVersion()` deref).
//
// We synthesise a `java.lang.Package` object with:
//   * `name` вЂ” the dotted package name (e.g. `org.keycloak.common`).
//   * `implementationTitle`/`implementationVersion`/`specificationTitle`/
//     `specificationVersion`/`specificationVendor`/`implementationVendor` вЂ”
//     read from the manifest, best-effort (see caveat below).
//
// The lookup is best-effort: if the class is loaded from the boot classpath
// (no manifest) or from a directory, we return a Package with name set and
// every other field null. That matches HotSpot, which always returns a
// non-null Package for a named class even when the manifest is missing.
//
// Layout: real-JDK `java.lang.Package` extends `java.lang.NamedPackage`
// (fields `name`, `module`) and itself declares only `versionInfo`
// (`Package$VersionInfo`, bundling specTitle/specVersion/specVendor/
// implTitle/implVersion/implVendor/sealBase) and `packageInfo` (`Class<?>`,
// the cached `<pkg>.package-info` lookup). There is NO direct `specTitle` /
// `implVersion` / etc. field on `Package` itself in JDK 9+ вЂ” those manifest
// attributes live inside the nested `VersionInfo` record.
//
// We therefore write fields ONLY by name, never by raw slot index (past
// slot 0, which happens to be `name` in both the synthetic-mode and
// real-JDK layouts). A prior version of this function also wrote the
// manifest attributes by raw index (1..6), on the mistaken assumption
// that `Package` declares six flat manifest fields at those slots. Under
// the real JDK 25 layout those indices are actually `module` (1),
// `versionInfo` (2), and `packageInfo` (3) вЂ” the index-3 write silently
// overwrote `packageInfo` with a `String` (the `Specification-Vendor`
// manifest value, whenever a jar's manifest set one), which is not a
// `Class`. Real `Package.getAnnotation()`/`isAnnotationPresent()` bytecode
// treats `packageInfo` as a `Class<?>` receiver unconditionally once it is
// non-null (`getPackageInfo()`'s lazy-load only fires when the field is
// still null) вЂ” so `pkg.getAnnotation(X)` on any such Package dispatched
// `getAnnotation` on a `String` receiver, throwing `NoSuchMethodError:
// java/lang/String.getAnnotation`. This is exactly what tripped up
// Arquillian's `EventTestRunnerAdaptor` bootstrap (MOXy JAXB asks
// `Package.isAnnotationPresent(XmlSchema.class)` on classes from
// JBoss-vendored jars whose manifests set `Specification-Vendor`). See
// `keycloak-arquillian-package-getannotation-string-linkage.md`.
//
// Consequence of writing by name only: since `Package` has no flat
// `specTitle`/etc. fields to target by name, `getSpecificationTitle()` and
// friends currently surface null even when the manifest has real values
// (rather than crashing, which is what the raw-index write did). Properly
// populating them would require constructing a real `Package$VersionInfo`
// via its private `getInstance(...)` factory, which needs a static-invoke
// capability this native layer doesn't currently expose вЂ” left as a
// follow-up. We do wire `versionInfo` to the `NULL_VERSION_INFO` sentinel
// (matching the real 2-arg `Package(String, Module)` constructor) so that
// sentinel, not a raw null, backs `getSpecificationTitle()` / `isSealed()`
// / etc. вЂ” avoiding a NullPointerException in their place.
// ---------------------------------------------------------------------------

/// Read a manifest attribute by name from the class's source jar, if any.
/// Returns `None` for classes loaded from the boot path. For Spring Boot
/// exploded archives, classes under `BOOT-INF/classes` and `WEB-INF/classes`
/// inherit the enclosing archive root's manifest.
///
/// Supports three CodeSource URL forms:
///   * `file:/C:/.../foo.jar`                              вЂ” plain jar
///   * `jar:file:/C:/.../outer.jar!/BOOT-INF/lib/inner.jar!/` вЂ” Spring Boot 2.x
///   * `jar:nested:/C:/.../outer.jar/!BOOT-INF/lib/inner.jar!/` вЂ” Spring Boot 3.x
///
/// For the nested forms, opens the outer jar, extracts the inner jar entry
/// to a byte buffer, reads `META-INF/MANIFEST.MF` from the inner zip, and
/// parses out the requested attribute. This is what unblocks
/// `SpringBootVersion.class.getPackage().getImplementationVersion()` for
/// Spring Boot fat-jar deployments where the class lives in a nested jar.
pub fn t19_h10_class_manifest_attr(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    attr: &str,
) -> Option<String> {
    // A Class mirror normally carries the exact CodeSource captured during
    // loading. Some real-class loader routes do not retain that mirror edge,
    // however; the class-path index is the authoritative fallback and still
    // resolves the JAR containing this precise internal class name.
    let class_name = ctx.class_name_of_id(class_id);
    let package_path = class_name.as_deref().and_then(|name| {
        name.rsplit_once('/')
            .map(|(package, _)| format!("{package}/"))
    });
    let code_base = ctx.class_code_base(class_id).unwrap_or_default();
    let url = if code_base.is_empty() || code_base.starts_with("class:") {
        class_name
            .as_deref()
            .and_then(|name| ctx.find_class_source_path(name))?
    } else {
        code_base.clone()
    };
    // Spring Boot nested-jar handling.
    // 2.x: `jar:file:/<outer>!/BOOT-INF/lib/<inner>.jar!/`
    // 3.x: `jar:nested:/<outer>/!BOOT-INF/lib/<inner>.jar!/`
    if let Some(rest) = url
        .strip_prefix("jar:file:")
        .or_else(|| url.strip_prefix("jar:nested:"))
    {
        // Split on the first `!/` (or `/!` for 3.x's `nested:` form which
        // separates the outer-jar path from the inner entry with `/!`).
        let (outer_part, inner_part) = if let Some(idx) = rest.find("!/") {
            (&rest[..idx], &rest[idx + 2..])
        } else if let Some(idx) = rest.find("/!") {
            (&rest[..idx], &rest[idx + 2..])
        } else {
            ("", "")
        };
        if !outer_part.is_empty() && !inner_part.is_empty() {
            // Strip trailing `!/` from inner, then split inner on first `!/`
            // (some forms include a trailing entry-name section).
            let inner_entry = inner_part
                .trim_end_matches('/')
                .trim_end_matches('!')
                .trim_end_matches('/');
            // Strip leading slashes from outer path; keep the rest as a path.
            let outer_path = outer_part.trim_start_matches('/');
            let outer_pb = if cfg!(windows) {
                std::path::PathBuf::from(outer_path)
            } else {
                std::path::PathBuf::from(format!("/{}", outer_path))
            };
            if outer_pb.is_file() {
                if let Some(val) = nested_jar_manifest_attr(
                    ctx,
                    &outer_pb,
                    inner_entry,
                    package_path.as_deref(),
                    attr,
                ) {
                    return Some(val);
                }
            }
        }
        // Fall through to None if the nested form failed to resolve.
        return None;
    }

    // Plain `file:` URL (single jar). The parsed manifest is cached per VM
    // while the jar's stamp holds, so repeated `Class.getPackage()`
    // invocations that each query 6 attributes don't re-parse the same jar.
    let path = url.strip_prefix("file:").unwrap_or(&url);
    let path = path.trim_start_matches('/');
    let path = if cfg!(windows) {
        std::path::PathBuf::from(path)
    } else {
        std::path::PathBuf::from(format!("/{}", path))
    };
    if path.is_file() {
        return plain_jar_manifest_attr(ctx, &path, package_path.as_deref(), attr);
    }
    spring_boot_exploded_manifest_attr(ctx, &path, package_path.as_deref(), attr)
}

/// The stamp (length + modification time) of the file at `path`, `None` when
/// it cannot be `stat`ed.
pub fn package_manifest_file_stamp(path: &std::path::Path) -> Option<crate::classloader::FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// The parsed manifest `parse` reads for `key`, from THIS VM's cache
/// (`classloader::package_manifest_cache`) while the file at `stamp_path`
/// keeps the stamp the row was parsed under. A moved stamp drops the row and
/// parses again, so a jar rewritten on disk is read afresh by the next ask, as
/// HotSpot's next `JarFile` over it reads the new manifest. A file that
/// cannot be `stat`ed, and a context with no per-VM slot, parse for this ask
/// only. The `Class.getPackage()` lookups ask 6-7 attributes per call, so a
/// hit costs one `stat`.
///
/// Interpreter round i4 wave 99, lane L5
/// (`i98-orch-manifest-caches-are-process-wide-and-never-invalidated`): the
/// exploded, plain and nested caches were three process globals keyed by path
/// only -- shared by every VM in the process (AGENTS.md: no process globals
/// for compatibility state) and never invalidated.
pub fn cached_package_manifest(
    ctx: &dyn NativeContext,
    key: &str,
    stamp_path: &std::path::Path,
    parse: impl FnOnce() -> HashMap<String, String>,
) -> Arc<HashMap<String, String>> {
    let Some(stamp) = package_manifest_file_stamp(stamp_path) else {
        return Arc::new(parse());
    };
    let Some(cache) = crate::classloader::package_manifest_cache(ctx) else {
        return Arc::new(parse());
    };
    {
        let mut guard = cache.lock();
        let row = guard
            .get(key)
            .map(|(row_stamp, map)| (*row_stamp == stamp, Arc::clone(map)));
        match row {
            Some((true, map)) => return map,
            Some((false, _)) => {
                guard.remove(key);
            }
            None => {}
        }
    }
    // Read and parse outside the lock.
    let map = Arc::new(parse());
    // A file rewritten while it was read keeps no row: the next ask reads it again.
    if package_manifest_file_stamp(stamp_path) == Some(stamp) {
        let mut guard = cache.lock();
        if guard.len() >= crate::classloader::PACKAGE_MANIFEST_CACHE_LIMIT {
            guard.clear();
        }
        guard.insert(key.to_string(), (stamp, Arc::clone(&map)));
    }
    map
}

/// Spring Boot's exploded launcher gives its URLClassLoader
/// `.../BOOT-INF/classes` (or `WEB-INF/classes`) as the class path entry, but
/// package metadata is defined from the archive root's `META-INF/MANIFEST.MF`.
/// A plain directory has no such inheritance, so restrict this lookup to the
/// two Boot layouts rather than searching arbitrary parent directories.
pub fn spring_boot_exploded_manifest_attr(
    ctx: &dyn NativeContext,
    path: &std::path::Path,
    package_path: Option<&str>,
    attr: &str,
) -> Option<String> {
    if !path.is_dir() || path.file_name()?.to_string_lossy() != "classes" {
        return None;
    }
    let layout_dir = path.parent()?;
    let layout = layout_dir.file_name()?.to_string_lossy();
    if layout != "BOOT-INF" && layout != "WEB-INF" {
        return None;
    }
    let manifest = layout_dir.parent()?.join("META-INF").join("MANIFEST.MF");
    // Key and stamp: the exploded archive's own manifest file.
    let cache_key = format!("exploded:{}", manifest.display());
    let map = cached_package_manifest(ctx, &cache_key, &manifest, || {
        std::fs::read(&manifest)
            .ok()
            .map(|bytes| parse_package_manifest(&bytes))
            .unwrap_or_default()
    });
    manifest_attr_for_package(&map, package_path, attr)
}

/// A plain jar's manifest attribute, the parsed manifest kept in THIS VM's
/// cache by the jar's path and stamp ([`cached_package_manifest`]) so the 6
/// lookups per `Class.getPackage()` parse the jar once while it is unchanged.
pub fn plain_jar_manifest_attr(
    ctx: &dyn NativeContext,
    path: &std::path::Path,
    package_path: Option<&str>,
    attr: &str,
) -> Option<String> {
    let cache_key = format!("jar:{}", path.display());
    let map = cached_package_manifest(ctx, &cache_key, path, || {
        use std::io::Read;
        std::fs::File::open(path)
            .ok()
            .and_then(|file| {
                let mut archive = zip::ZipArchive::new(file).ok()?;
                let mut entry = archive.by_name("META-INF/MANIFEST.MF").ok()?;
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).ok()?;
                Some(parse_package_manifest(&bytes))            })
            .unwrap_or_default()
    });
    manifest_attr_for_package(&map, package_path, attr)
}

/// Nested-jar manifests are cached the same way ([`cached_package_manifest`]),
/// keyed by `(outer_jar_path, inner_jar_entry)` and stamped by the OUTER jar.
/// Each row is a fully-parsed map of main
/// and per-package MANIFEST.MF attributes (case-insensitive lookups handled by
/// lowercasing keys at insertion time). This is essential for Spring Boot
/// fat-jar startup: `Class.getPackage()` queries 6 manifest attributes per
/// invocation, and Tomcat's `StringManager.getManager(Class)` calls
/// `getPackage()` once per package level for every class it touches.
/// Without caching, each call re-opens the outer 30-100MB JAR, parses its
/// central directory, extracts an inner ~5MB JAR into memory, parses ITS
/// central directory, and reads the MANIFEST.MF вЂ” taking 1-3 seconds each.
/// That blows up to minute-scale wait times during Tomcat init and is what
/// caused `insurance-backend` to never reach the Spring "Started" banner
/// within the 60s timeout.
///
/// Open the outer jar, extract the inner-jar entry into memory, and read
/// the requested `META-INF/MANIFEST.MF` attribute from inside the inner jar.
/// Returns `None` on any I/O / format failure (best-effort). The parsed
/// manifest is cached per `(outer_jar, inner_entry)` while the outer jar's
/// stamp holds, so repeated lookups of different attributes on the same
/// nested jar -- the common case during Spring Boot startup -- cost a map
/// probe and one `stat`.
pub fn nested_jar_manifest_attr(
    ctx: &dyn NativeContext,
    outer_jar: &std::path::Path,
    inner_entry: &str,
    package_path: Option<&str>,
    attr: &str,
) -> Option<String> {
    let entry_name = inner_entry.trim_start_matches('/');
    let cache_key = format!("nested:{}!{}", outer_jar.display(), entry_name);
    let parsed = cached_package_manifest(ctx, &cache_key, outer_jar, || {
        parse_nested_jar_manifest(outer_jar, entry_name).unwrap_or_default()
    });
    manifest_attr_for_package(&parsed, package_path, attr)
}

pub fn parse_nested_jar_manifest(
    outer_jar: &std::path::Path,
    entry_name: &str,
) -> Option<HashMap<String, String>> {
    use std::io::Read;
    let file = std::fs::File::open(outer_jar).ok()?;
    let mut outer = zip::ZipArchive::new(file).ok()?;
    let mut inner_bytes: Vec<u8> = Vec::new();
    {
        let mut entry = outer.by_name(entry_name).ok()?;
        entry.read_to_end(&mut inner_bytes).ok()?;
    }
    // Spring Boot 2.x stores BOOT-INF/lib/*.jar uncompressed (STORED) so we
    // can read it directly as a zip from the byte buffer.
    let cursor = std::io::Cursor::new(inner_bytes);
    let mut inner = zip::ZipArchive::new(cursor).ok()?;
    let mut mf_str = String::new();
    {
        let mut mf = inner.by_name("META-INF/MANIFEST.MF").ok()?;
        mf.read_to_string(&mut mf_str).ok()?;
    }
    Some(parse_package_manifest(mf_str.as_bytes()))
}

/// Parse a JAR manifest's main section plus its named package sections.
/// A flattened key has the form `"<package>\\0<attribute>"`; main-section
/// keys use an empty package prefix. Package values override main values,
/// exactly as `java.lang.Package` specifies.
pub fn parse_package_manifest(bytes: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let mut result = HashMap::new();
    let mut section: Vec<String> = Vec::new();

    let mut commit_section = |section: &mut Vec<String>| {
        if section.is_empty() {
            return;
        }
        let mut attrs = HashMap::new();
        for line in section.drain(..) {
            if let Some((key, value)) = line.split_once(": ") {
                attrs.insert(key.to_ascii_lowercase(), value.to_string());
            }
        }
        let name = attrs.remove("name").unwrap_or_default();
        for (key, value) in attrs {
            result.insert(format!("{name}\0{key}"), value);
        }
    };

    for line in text.lines() {
        if line.is_empty() {
            commit_section(&mut section);
        } else if let Some(continuation) = line.strip_prefix(' ') {
            if let Some(previous) = section.last_mut() {
                previous.push_str(continuation);
            }
        } else {
            section.push(line.to_string());
        }
    }
    commit_section(&mut section);
    result
}

pub fn manifest_attr_for_package(
    attributes: &HashMap<String, String>,
    package_path: Option<&str>,
    attr: &str,
) -> Option<String> {
    let attr = attr.to_ascii_lowercase();
    package_path
        .and_then(|package| attributes.get(&format!("{package}\0{attr}")).cloned())
        .or_else(|| attributes.get(&format!("\0{attr}")).cloned())
}

/// Real `Class.getPackage()` native вЂ” returns a `java.lang.Package` mirror
/// or null when the class has no resolvable package (primitive / array of
/// primitive). Always non-null for a real reference type.
/// Materialise the JDK 9+ `Package$VersionInfo` record that backs every
/// manifest-derived Package accessor. The real `Package` has no flat
/// `implVersion` field, so direct writes on the Package object are discarded.
pub fn package_version_info(
    ctx: &mut dyn NativeContext,
    spec_title: Option<String>,
    spec_version: Option<String>,
    spec_vendor: Option<String>,
    impl_title: Option<String>,
    impl_version: Option<String>,
    impl_vendor: Option<String>,
    seal_base: Option<String>,
) -> Result<Option<Value>, MethodCallFailed> {
    if spec_title.is_none()
        && spec_version.is_none()
        && spec_vendor.is_none()
        && impl_title.is_none()
        && impl_version.is_none()
        && impl_vendor.is_none()
        && seal_base.is_none()
    {
        return Ok(None);
    }

    // `VersionInfo` has no superclass fields: its six strings are slots 0..5
    // in the exact declaration order below, followed by `sealBase` at slot 6.
    // `set_field_by_name` can be a no-op before real JDK field metadata is
    // reflected, so use this stable layout rather than silently losing values.
    let info = try_alloc_concurrent_synthetic(ctx, "java/lang/Package$VersionInfo", 7)?;
    let info_pin = ctx.pin_native_root(info);
    for (slot, value) in [
        spec_title,
        spec_version,
        spec_vendor,
        impl_title,
        impl_version,
        impl_vendor,
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(value) = value {
            let string = ctx.create_string(&value);
            let info = ctx.read_native_pin(info_pin, info);
            ctx.set_field(info, slot, Value::Object(Some(string)));
        }
    }
    // `sealBase` (slot 6): the code source URL of a sealed package, what
    // `Package.isSealed()` / `isSealed(URL)` read.
    if let Some(base) = seal_base {
        if let Some(url) = new_jdk_url(ctx, &base)? {
            let info = ctx.read_native_pin(info_pin, info);
            ctx.set_field(info, 6, Value::Object(Some(url)));
        }
    }
    let info = ctx.read_native_pin(info_pin, info);
    ctx.unpin_native_roots(info_pin);
    Ok(Some(Value::Object(Some(info))))
}

/// `BuiltinClassLoader.isSealed(pn, man)`: `Sealed` in the package's own
/// manifest section, else the main section, equal to `true` ignoring case;
/// the seal base is then the code source URL `definePackage(pn, man, url)`
/// was given -- a class-path JAR's `file:` URL here. `None` when the package
/// is not sealed (or its code source is not a plain JAR). Every manifest
/// attribute but this one was already read; `Package.isSealed()` answered
/// `false` for a sealed JAR's package in both modes (interpreter round i2
/// wave 57, lane L5; probe `L5W57SealedJarPackage`).
pub fn package_seal_base(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<String> {
    let sealed = t19_h10_class_manifest_attr(ctx, class_id, "Sealed")?;
    if !sealed.trim().eq_ignore_ascii_case("true") {
        return None;
    }
    let code_base = ctx.class_code_base(class_id)?;
    (code_base.starts_with("file:") && code_base.ends_with(".jar")).then_some(code_base)
}

/// `new URL(spec)` through the JDK's constructor (handler included), or
/// `None` when the class library has no such constructor.
pub fn new_jdk_url(
    ctx: &mut dyn NativeContext,
    spec: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if ctx.is_class_synthetic_stub("java/net/URL")
        || !ctx.method_exists("java/net/URL", "<init>", "(Ljava/lang/String;)V")
    {
        return Ok(None);
    }
    let Some(Value::Object(Some(url))) = ctx.new_object("java/net/URL")? else {
        return Ok(None);
    };
    let pin = ctx.pin_native_root(url);
    let s = ctx.create_string_uninterned(spec);
    let url = ctx.read_native_pin(pin, url);
    let ran = ctx.invoke_special(
        "java/net/URL",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(url)), Value::Object(Some(s))],
    );
    let url = ctx.read_native_pin(pin, url);
    ctx.unpin_native_roots(pin);
    ran?;
    Ok(Some(url))
}

pub fn native_class_get_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = ctx.class_id_from_mirror(this);
    // Synthetic lambda proxies deliberately have no class-store entry, so
    // `mirror_class_name` cannot resolve their generated ClassId. Their
    // defining host is the authoritative package source, just as it is for
    // `getPackageName()` above. Preserve the non-null `Package` contract for
    // callers (such as Spring GraphQL) that immediately dereference it.
    let lambda_pkg = class_id
        .and_then(|class_id| ctx.lambda_proxy_host(class_id))
        .map(|host| match host.rfind('/') {
            Some(pos) => Arc::from(host[..pos].replace('/', ".")),
            None => Arc::from(""),
        });
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    // Array classes have no Package object; Class.getPackageName() separately
    // reports the component package (java.lang for primitive arrays).
    if (name.is_empty() && lambda_pkg.is_none()) || name.starts_with('[') {
        return Ok(Some(Value::Object(None)));
    }
    // Nor do primitive classes: the JDK's body opens with `if (isPrimitive()
    // || isArray()) return null;`. `int.class`'s name has no `/`, so it fell
    // into the default-package arm below and answered a Package named ""
    // (every mode; probe `tools/probes/interp/L5/L5W44LoaderReview.java`,
    // `array-package`; interpreter round i1 wave 45, lane L5).
    if lambda_pkg.is_none()
        && !name.contains('/')
        && matches!(
            native_class_is_primitive(ctx, &[Value::Object(Some(this))]),
            Ok(Some(Value::Int(1)))
        )
    {
        return Ok(Some(Value::Object(None)));
    }
    // Cache the dotted package prefix per `ClassId` for VM-registered
    // mirrors (class_id_from_mirror = Some). Synthetic / test mirrors
    // derive on-call so tests are not contaminated.
    let pkg_name: Arc<str> = if let Some(pkg_name) = lambda_pkg {
        pkg_name
    } else if let Some(class_id) = class_id {
        if let Some(arc) = cache_get(&PACKAGE_NAME_CACHE, ctx.vm_identity(), class_id) {
            arc
        } else {
            package_name_of(ctx.vm_identity(), class_id, &name)
        }
    } else if let Some(pos) = name.rfind('/') {
        Arc::from(name[..pos].replace('/', "."))
    } else {
        // Default package вЂ” return a Package object whose name is the empty
        // string, matching JDK 25 (`Class.forName("Foo").getPackage()`
        // yields a Package with `getName().equals("")`).
        Arc::from("")
    };
    // HotSpot interns exactly one `Package` per (defining loader, package
    // name): `Foo.class.getPackage() == Bar.class.getPackage()` for two classes
    // in the same package, and it is the same object
    // `ClassLoader.getDefinedPackage(name)` hands back. Share
    // `defined_package_memo` with the ClassLoader-side builder so both hold.
    //
    // A memo entry is FULLY BUILT (`rich`) once this function has populated it.
    // `getDefinedPackage` can get there first, though, and it can only
    // synthesise the thin name+module+NULL_VERSION_INFO shape — so on a thin
    // hit we ENRICH that very object in place (same `java/lang/Package`
    // 12-field synthetic layout) rather than allocating a second one, which
    // would either lose this class's manifest attributes or break identity.
    // A rich hit is returned untouched: HotSpot likewise never re-derives a
    // Package once defined, so a same-package class from a different jar does
    // not overwrite the first definer's manifest attributes.
    // gen r4w3/rooting: the allocation / `Package$VersionInfo` <clinit> /
    // version-info construction below are GC points and `this` is read again
    // afterwards (manifest lookup, `getModule` receiver); pin it here and
    // re-read before each use. Released together with `pkg_pin` below.
    let this_pin = ctx.pin_native_root(this);
    let pkg_ns = match class_id.map(|cid| package_memo_ns_of_class(ctx, cid)) {
        // gc-common w31: a class the VM tags bootstrap (`0`) can still be
        // defined, as far as Java is concerned, by the PLATFORM loader --
        // `java.sql.Connection.class.getClassLoader()` answers it -- and the
        // platform loader's `getDefinedPackage` keys namespace `1`. Ask the
        // class's own loader object, as `getDefinedPackage` does (a JDK class
        // only; `getClassLoader` answers null for a real bootstrap class).
        Some(0) => {
            let mirror = ctx.read_native_pin(this_pin, this);
            match native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))]) {
                Ok(Some(Value::Object(Some(loader)))) => {
                    crate::classloader::loader_namespace_id(ctx, loader)
                }
                _ => 0,
            }
        }
        Some(ns) => ns,
        None => 0,
    };
    // The boot loader defines no `Package` for the UNNAMED package of a class
    // appended to its search (`-Xbootclasspath/a`, an agent's
    // `Boot-Class-Path`): HotSpot's `Class.getPackage` is
    // `BootLoader.definePackage(this)`, `getDefinedPackage("")`, which answers
    // null for it. Interpreter round i2 wave 56, lane L5 (probe
    // `L5W56BootAppendPackages`, `self-pkg`;
    // `docs/feature-designs/native-dispatch-architecture.md`).
    if pkg_ns == 0
        && pkg_name.is_empty()
        && !name.is_empty()
        && ctx.is_bootstrap_appended_class(&name)
    {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(None)));
    }
    // A user loader's own `packages` map first. HotSpot's `Class.getPackage`
    // is the defining loader's `definePackage(this)`, which answers the
    // `Package` that map holds -- the one `URLClassLoader.defineClass`'s
    // `definePackage(name, manifest, url)` put there, manifest attributes
    // included -- and `getDefinedPackage` answers the same object
    // (`i2_classloader_get_defined_package`'s first step). The memo below
    // minted a second one beside it (interpreter round i2 wave 53, lane L5;
    // probe `L5W53PackageIdentity`, `defined`). A lazy `NamedPackage` entry
    // keeps the memo route, which `getDefinedPackage` shares.
    if pkg_ns >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        let mirror = ctx.read_native_pin(this_pin, this);
        if let Ok(Some(Value::Object(Some(loader)))) =
            native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))])
        {
            if let Some(RealPackageEntry::Package(pkg)) =
                real_defined_package(ctx, Some(loader), &pkg_name)
            {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(pkg))));
            }
        }
    }
    // `CRATONVM_JDK_BUILT_PACKAGES`: the application loader's `Package` for a
    // class-path class is the JDK's `BuiltinClassLoader.definePackage(pn,
    // man, url)`'s, in that loader's own map (interpreter round i2 wave 58,
    // lane L5). One flag read on this path when the switch is off.
    if pkg_ns == cratonvm_types::ClassLoaderId::NATIVE_APPLICATION && ctx.jdk_built_packages() {
        if let Some(class_id) = class_id {
            let mirror = ctx.read_native_pin(this_pin, this);
            if let Some(pkg) = jdk_built_class_path_package(ctx, mirror, class_id, &pkg_name) {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(pkg))));
            }
        }
    }
    let memo_key = (ctx.vm_identity(), pkg_ns, pkg_name.to_string());
    // Current: the memo's rows are re-addressed after every moving collection
    // ([`gc_update_defined_package_refs`], gc-common w31-a).
    let memo_hit = defined_package_memo(crate::classloader::loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&memo_key)
        .map(|row| (row.pkg, row.rich));
    // Carry the memo KEY, not the ObjectRef, across the manifest reads below:
    // those can allocate, and a moving collection would leave the `ObjectRef`
    // stale. The row is remapped, so re-reading it immediately before use is
    // always safe.
    let reuse_thin = match memo_hit {
        Some((pkg, true)) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(pkg))));
        }
        Some((_, false)) => true,
        None => false,
    };

    // Read manifest attributes (best-effort) -- for a built-in loader's class
    // only. A user loader's class reaches this point when its loader's
    // `packages` map holds no `Package` for it (none, or the lazy
    // `NamedPackage` `ClassLoader.postDefineClass` puts there): its loader
    // never called `definePackage(name, manifest, url)` (a plain
    // `defineClass`, whatever its `ProtectionDomain`'s code source). HotSpot's
    // `Class.getPackage` is then `ClassLoader.definePackage(Class)` ->
    // `NamedPackage.toPackage`, a `Package` with `NULL_VERSION_INFO`: it never
    // reads the class's jar. Reading it here answered the jar's
    // `Implementation-Version` where HotSpot answers null (`--compatible`; the
    // JDK body runs under `--real-jdk`). Interpreter round i4 wave 99, lane L5
    // (probe `L5W99ManifestRewritten`, `define` rows).
    //
    // Except where the VM's own `URLClassLoader.findClass` stand-in defined
    // the class (`ucl_try_define_local_class`: the `JdkUrlClassLoaders` row
    // off, or no real JDK): the JDK's `URLClassLoader.defineClass` it stands
    // in for runs `definePackage(name, manifest, url)` first, which the
    // stand-in does not, so the jar's manifest is still the answer there
    // (interpreter round i5 wave 100, lane L5).
    let reads_manifest = pkg_ns < cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED
        || (!ctx.jdk_url_class_loaders() && {
            let mirror = ctx.read_native_pin(this_pin, this);
            match native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))]) {
                Ok(Some(Value::Object(Some(loader)))) => {
                    crate::classloader::find_class_is_urlclassloader_native(ctx, loader)
                }
                _ => false,
            }
        });
    let this = ctx.read_native_pin(this_pin, this);
    let (impl_title, impl_version, spec_title, spec_version, spec_vendor, impl_vendor, seal_base) =
        if let Some(class_id) = mirror_class_id(ctx, this).filter(|_| reads_manifest) {
            (
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Vendor"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Vendor"),
                package_seal_base(ctx, class_id),
            )
        } else {
            (None, None, None, None, None, None, None)        };
    let thin = if reuse_thin {
        defined_package_memo(crate::classloader::loader_tables(ctx))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&memo_key)
            .map(|row| row.pkg)
    } else {
        None
    };
    let pkg = match thin {
        Some(pkg) => pkg,
        None => try_alloc_concurrent_synthetic(ctx, "java/lang/Package", 12)?,
    };
    let pkg_pin = ctx.pin_native_root(pkg);
    // Slot 0: name (synthetic-mode layout used by `getPackageName`/`getName`
    // shims pre-real-class-load). Also happens to be `NamedPackage.name`'s
    // real slot, so the by-index write is harmless there; every other field
    // below is by-name ONLY вЂ” see the raw-index corruption note above.
    // Interned: `NamedPackage.<init>` stores `pn.intern()` (round 14 wave 3).
    let name_str = ctx.intern_string(&pkg_name);
    let pkg = ctx.read_native_pin(pkg_pin, pkg);
    ctx.set_field(pkg, 0, Value::Object(Some(name_str)));
    ctx.set_field_by_name(pkg, "name", Value::Object(Some(name_str)));
    // Manifest-derived attributes. NOT written by raw slot index (see the
    // function-level comment above) вЂ” `Package` has no flat `specTitle` /
    // `implVersion` / etc. fields in real JDK 9+, so these by-name writes
    // currently no-op for a real-class Package (tracked as a follow-up);
    // what matters here is that they can never clobber `module` /
    // `versionInfo` / `packageInfo` the way the old by-index writes did.
    if let Ok(Some(version_info)) = package_version_info(
        ctx,
        spec_title,
        spec_version,
        spec_vendor,
        impl_title,
        impl_version,
        impl_vendor,
        seal_base,
    ) {
        // NamedPackage contributes name and module at slots 0 and 1; the
        // real JDK Package.versionInfo field is consequently slot 2.
        let pkg = ctx.read_native_pin(pkg_pin, pkg);
        ctx.set_field(pkg, 2, version_info);
    } else if let Ok(vi_cid) = ctx.ensure_class_initialized("java/lang/Package$VersionInfo") {
        if let Some(idx) = ctx.static_field_index_by_name(vi_cid, "NULL_VERSION_INFO") {
            let null_version_info = ctx.get_static_field(vi_cid, idx);
            if matches!(null_version_info, Value::Object(Some(_))) {
                let pkg = ctx.read_native_pin(pkg_pin, pkg);
                ctx.set_field(pkg, 2, null_version_info);
            }
        }
    }
    // Wire `versionInfo` to the real `Package$VersionInfo.NULL_VERSION_INFO`
    // sentinel, matching what the real `Package(String, Module)` constructor
    // does unconditionally (`javap -c` confirms `getstatic
    // Package$VersionInfo.NULL_VERSION_INFO; putfield versionInfo`). Without
    // this, `versionInfo` stays null on our hand-built object and
    // `getSpecificationTitle()` / `isSealed()` / etc. (which all deref
    // `versionInfo` unconditionally) throw NullPointerException instead of
    // HotSpot's null/false. Best-effort: if `Package$VersionInfo` can't be
    // resolved (e.g. pure synthetic-JDK mode with no real `java.lang.Package`
    // on the classpath), leave `versionInfo` unset вЂ” real bytecode isn't
    // running against this object in that mode anyway.
    // Wire the Package's `module` from the class's own module so the *real*
    // `Package.getDeclaredAnnotations()` bytecode works. That JDK body does
    // `packageInfo()` -> `module().getClassLoader()`; with a null module it
    // throws `NullPointerException: Cannot invoke getClassLoader on null`,
    // which aborted EVERY `orm.xml`/mapping-document parse (glassfish JAXB's
    // `RuntimeInlineAnnotationReader.getPackageAnnotation` reads the package's
    // `@XmlSchema`). With the module set, `packageInfo()` resolves the
    // (usually absent) `<pkg>.package-info` -> sentinel -> `[]`, matching
    // HotSpot. `Class.getModule()` already returns a valid (unnamed) module.
    // gen r4w3/rooting: current receiver after the GC points above.
    let this = ctx.read_native_pin(this_pin, this);
    let module_val = match ctx.invoke_virtual(this, "getModule", "()Ljava/lang/Module;", &[]) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        // `getModule()` can still answer null for a mirror with no class-store
        // entry (synthetic lambda proxies, hand-built test mirrors). Leaving
        // `module` unset is never acceptable -- see `canonical_unnamed_module`
        // -- so fall back to the canonical unnamed module.
        _ => Value::Object(Some(canonical_unnamed_module(ctx)?)),
    };
    if matches!(module_val, Value::Object(Some(_))) {
        let pkg = ctx.read_native_pin(pkg_pin, pkg);
        ctx.set_field_by_name(pkg, "module", module_val);
    }
    // Eagerly resolve `<pkg>.package-info` and cache it in the Package's
    // `packageInfo` field so the real `Package.getDeclaredAnnotations()` /
    // `getAnnotation()` / `isAnnotationPresent()` bytecode (all of which
    // delegate to `getPackageInfo()`) surfaces package-level annotations such
    // as JSpecify `@NullMarked` / `@NullUnmarked`. Without this, `getPackageInfo()`
    // calls `Class.forName(<pkg>.package-info, false, module.getClassLoader())`
    // with a *null* loader (our unnamed module reports no class loader), fails
    // to locate the class, and caches the empty `PackageInfoProxy` sentinel вЂ”
    // leaving every package annotation invisible. Loading via the same path the
    // declaring class used works (best-effort: most packages have no
    // package-info, in which case `load_class` errors and we leave the field
    // null so the real fallback runs, matching HotSpot's empty result).
    if !pkg_name.is_empty() {
        let pi_internal = format!("{}/package-info", pkg_name.replace('.', "/"));
        // `package-info` is the class the DECLARING class's loader answers
        // (`Package.getPackageInfo` asks `module.getClassLoader()`), not the
        // sole global copy: a fork loader's `Package` otherwise cached the
        // application loader's `package-info`, whose annotation types are the
        // application's (`EhCache3CacheAutoConfigurationTests`: JAXB read the
        // `@XmlSchema` of the wrong loader and lost the namespace). Fall back
        // to the loader-blind load when the loader cannot be asked.
        let declaring_ns = class_id.map(|cid| ctx.loader_id_of_class(cid));
        let via_loader = match class_id {
            Some(cid) => ctx
                .class_id_by_name_via_referencing_class(cid, &pi_internal)
                .ok()
                .map(|pi| Value::Object(Some(ctx.get_class_mirror(pi)))),
            None => None,
        };
        let resolved = match via_loader {
            Some(v) => Some(v),
            // A miss through a user-defined loader is its answer (no
            // `package-info`); only a built-in or unknown owner may fall back
            // to the loader-blind load.
            None if declaring_ns.is_some_and(|ns| ns >= 3) => None,
            None => match ctx.load_class(&pi_internal) {
                Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
                _ => None,
            },
        };
        if let Some(v) = resolved {
            let pkg = ctx.read_native_pin(pkg_pin, pkg);
            ctx.set_field_by_name(pkg, "packageInfo", v);
        }
    }
    let pkg = ctx.read_native_pin(pkg_pin, pkg);
    // gen r4w3/rooting: `this_pin` is below `pkg_pin`; releasing it releases both.
    ctx.unpin_native_roots(this_pin);
    // Publish (or promote to `rich`) so the next `getPackage()` /
    // `getDefinedPackage()` for this package returns this same object. An
    // existing thin entry upgraded in place keeps its row (and its owner).
    // gc-common w31-a: no GC point from the last pin read above to the insert,
    // so `pkg` and the owner's loader are current when the row records them.
    let owner = defined_package_owner(ctx, pkg_ns, class_id);
    let tables = crate::classloader::loader_tables(ctx);
    publish_defined_package(tables, memo_key, pkg, true, owner);
    Ok(Some(Value::Object(Some(pkg))))
}

// ---------------------------------------------------------------------------
// I2 вЂ” ClassLoader package-management overrides
//
// Background: in real-JDK mode the JDK's `ClassLoader` Java code reads its
// private `packages: ConcurrentHashMap<String, NamedPackage>` field from
// `getDefinedPackage`, `getNamedPackage`, and `definePackage(String,Module)`.
// User-instantiated ClassLoader subclasses (e.g. ByteBuddy's
// `JavaDispatcher$DynamicClassLoader`) end up with this field null in our
// VM because the private constructor's `putfield #13 packages` write is
// not landing on the right slot/field for subclasses.  The first reflective
// or `Class.getPackage()` call on a class loaded by such a loader walks
// into `getDefinedPackage` -> `packages.get(name)` -> NPE.
//
// ByteBuddy's `JavaDispatcher.<clinit>` triggers exactly this chain
// (`ForModuleSystem.accept` -> `Invoker.class.getPackage()` -> a
// JDK-internal `getDefinedPackage` lookup) and the resulting NPE
// propagates up as `IllegalStateException: Failed to create invoker for
// Invoker` -- the canonical I2 blocker for ByteBuddy + Mockito.
//
// Fix: register native bodies for the three methods below so they avoid the
// null `packages` field. `getDefinedPackage` derives a conservative answer
// from classpath-visible class files; the package builders synthesize the
// JDK object shape so `postDefineClass` and `Class.getPackage()` keep working
// without depending on `packages` being non-null.

/// Per-(loader-namespace-id, package-name) memo of the one `java.lang.Package`
/// object this VM hands out — shared by [`native_class_get_package`]
/// (`Class.getPackage()`) and [`i2_classloader_get_defined_package`]
/// (`ClassLoader.getDefinedPackage`), which the JDK guarantees return the same
/// instance.
///
/// Value is a [`DefinedPackageRow`]: the `Package` itself, whether it is
/// `rich`, and its owner. `rich` is `false` for the thin
/// name+module+`NULL_VERSION_INFO` shape the ClassLoader side can build with no
/// class mirror to read a manifest from, and `true` once `Class.getPackage()`
/// has populated the manifest attributes / `packageInfo` on it. A thin entry is
/// upgraded in place, never replaced, so identity survives the upgrade.
///
/// gc-common w31-a (`common-w30b-defined-package-memo-pins-user-loaders`): the
/// row holds the `Package` itself, reported by the `defined-packages` row of
/// `vm/src/memory/native_roots.rs` ([`gc_scan_defined_package_roots`]) and
/// re-addressed after every moving collection
/// ([`gc_update_defined_package_refs`]). It used to hold a strong JNI global
/// root, and a user loader's `Package` reaches its loader (`module` is the
/// loader's module, `packageInfo` one of its classes), so one
/// `Class.getPackage()` or `ClassLoader.getDefinedPackage()` kept the loader --
/// and every class it defined -- for the life of the VM. Under the cycle's
/// loader-conditional licence a user loader's row is now pinned to that loader
/// (`metadata_pin`), as the JDK's `loader.packages` map holds it; a built-in
/// namespace's row, and every row on a cycle without the licence, is rooted.
/// A row goes when its loader's weak lock key is freed
/// ([`forget_lang_class_keys`]), when the class it was built for unloads
/// ([`forget_unloaded_defined_packages`]), and at teardown
/// ([`forget_vm_lang_class_tables`]). Identity holds while the loader lives,
/// which is all the JDK promises.
///
/// Two bugs this fixes, both measured against HotSpot on the
/// `BeanDefinitionLoaderTests` classpath:
/// * `cl.getDefinedPackage(p) == cl.getDefinedPackage(p)` was `false`, and
///   `c.getPackage() == cl.getDefinedPackage(c.getPackageName())` was `false` —
///   a fresh `Package` was synthesised per call. The JDK returns one identity
///   per (loader, package), and code that keys an `IdentityHashMap` on a
///   `Package`, or compares with `==`, silently diverged.
/// * `getDefinedPackages()` returned an empty array while
///   `getDefinedPackage(name)` returned non-null for the same loader — a
///   self-contradictory pair.
///
/// Keyed `(vm_identity, namespace, package)` (gc-common w21-d). The rows'
/// objects live in ONE VM's heap, and the namespace numbering starts over in
/// every VM (`0` is every VM's built-in namespace), so the old
/// `(namespace, package)` key let a second VM in the process answer the first
/// VM's row (it then held a global-root handle, which the second VM resolved in
/// its OWN table). Rows leave with their VM through
/// [`forget_vm_lang_class_tables`].
pub type DefinedPackageKey = (usize, u32, String);

/// One [`defined_package_memo`] row (gc-common w31-a).
#[derive(Clone, Copy)]
pub struct DefinedPackageRow {
    /// The one `Package` for the key. Current after every moving collection
    /// ([`gc_update_defined_package_refs`]).
    pub pkg: ObjectRef,
    /// `Class.getPackage()` has populated it (see [`defined_package_memo`]).
    pub rich: bool,
    /// The user loader that owns the row, with its weak lock key; `None` for a
    /// built-in namespace (rooted). NOT a root: kept so the root scan can pin
    /// `pkg` to it, re-addressed by [`gc_update_defined_package_refs`], and
    /// the row is dropped when the key is freed ([`forget_lang_class_keys`]).
    pub loader: Option<(ObjectRef, usize)>,
    /// The class `Class.getPackage()` built the row for, user namespace only:
    /// its unload hint drops the row ([`forget_unloaded_defined_packages`]),
    /// and without a `loader` the scan pins by it (`defer_or_root`).
    pub class_owner: Option<u32>,
    /// A built-in loader's `packages` map holds an entry for the key
    /// ([`sync_builtin_loader_packages`] put `pkg` there, or found one), so
    /// the next listing skips it without a Java call.
    pub in_loader_map: bool,
}

/// One VM's defined-`Package` memo: a field of its
/// [`LoaderIdentityTables`](crate::classloader::LoaderIdentityTables)
/// (`LoaderIdentityTables::defined_packages`), so it dies with the VM.
///
/// A process `static` keyed by VM identity until interpreter round i5 wave
/// 105, lane L5 (`i101-L5-remaining-loader-family-process-globals`). The key
/// keeps its VM element (every row of one instance carries the same one), so
/// the ~25 readers kept their key shape. The two lock-key sweeps reach no
/// VM's tables, so a dead user loader's rows are no longer dropped from
/// [`forget_lang_class_keys`]: the VM calls
/// [`forget_dead_loader_defined_packages`] right after each of its sweeps.
pub struct DefinedPackageMemo {
    pub rows: Mutex<HashMap<DefinedPackageKey, DefinedPackageRow>>,
    pub builtin_sync_locks: Mutex<
        HashMap<DefinedPackageKey, std::sync::Arc<Mutex<()>>>,
    >,
}

impl DefinedPackageMemo {
    pub fn new() -> Self {
        Self {
            rows: Mutex::new(HashMap::new()),
            builtin_sync_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Drop every row (a detached instance's identity teardown).
    pub fn clear(&self) {
        self.rows.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.builtin_sync_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

pub fn defined_package_memo(
    tables: &crate::classloader::LoaderIdentityTables,
) -> &Mutex<HashMap<DefinedPackageKey, DefinedPackageRow>> {
    &tables.defined_packages().rows
}

/// The package map and the memo must publish the same first `Package` under
/// concurrent first loads. A per-VM, per-package lock spans the short Java
/// `get`/`putIfAbsent` sequence; no process-global compatibility state.
pub fn builtin_loader_package_sync_lock(
    tables: &crate::classloader::LoaderIdentityTables,
    key: &DefinedPackageKey,
) -> std::sync::Arc<Mutex<()>> {
    let mut locks = tables
        .defined_packages()
        .builtin_sync_locks
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::sync::Arc::clone(
        locks
            .entry(key.clone())
            .or_insert_with(|| std::sync::Arc::new(Mutex::new(()))),
    )
}

/// After a lock-key sweep of the VM that owns `tables`
/// (`gc_sweep_lock_keys`, `gc_sweep_and_remap_lock_keys`,
/// `gc_drop_lock_keys_in_spans`, each answering a non-zero drop count): drop
/// the memo rows whose owning user loader's weak lock key that sweep freed.
/// Their `Package`s were pinned to that loader and are gone with it; a row
/// left behind would name a reclaimed object at the next root scan (gc-common
/// w31-a; this was the `defined_package_memo` arm of
/// [`forget_lang_class_keys`] until wave 105).
///
/// A key is freed when the registry no longer names it
/// (`crate::lock_key_owner_vm`): a swept weak slot is removed, never
/// tombstoned. The memo lock and the registry's shard locks are taken one at
/// a time, never nested; nothing here reaches Java. Answers the rows dropped.
pub fn forget_dead_loader_defined_packages(
    tables: &crate::classloader::LoaderIdentityTables,
) -> usize {
    let memo = defined_package_memo(tables);
    // addr-keyed: not an address -- lock-key registry keys, local.
    let named: rustc_hash::FxHashSet<usize> = {
        let guard = memo.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_empty() {
            return 0;
        }
        guard
            .values()
            .filter_map(|row| row.loader.map(|(_, key)| key))
            .collect()
    };
    // addr-keyed: not an address -- lock-key registry keys, local.
    let freed: rustc_hash::FxHashSet<usize> = named
        .into_iter()
        .filter(|&key| crate::lock_key_owner_vm(key as u64).is_none())
        .collect();
    if freed.is_empty() {
        return 0;
    }
    let mut guard = memo.lock().unwrap_or_else(|e| e.into_inner());
    let before = guard.len();
    guard.retain(|_, row| !row.loader.is_some_and(|(_, key)| freed.contains(&key)));
    before - guard.len()
}

/// The owner a [`defined_package_memo`] row records: `(loader, class)`.
///
/// Both `None` for a built-in namespace (`ns < 3`: the bootstrap, platform and
/// application loaders, and every built-in collapsed to `0`), whose rows stay
/// rooted. For a user namespace, the loader OBJECT of that namespace (this VM's
/// namespace store, else the defining loader of `class_id`) with a weak lock
/// key minted for it, and `class_id` as the class owner.
///
/// Takes no GC point: the loader is read from a remapped store, and its key is
/// minted at that (current) address. The caller must publish the row with no
/// GC point in between. Mints one weak lock key per new row (the lock-key
/// registry mutex), never on a memo hit.
pub fn defined_package_owner(
    ctx: &dyn NativeContext,
    ns: u32,
    class_id: Option<ClassId>,
) -> (Option<(ObjectRef, usize)>, Option<u32>) {
    if ns < cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        return (None, None);
    }
    let tables = crate::classloader::loader_tables(ctx);
    let class_owner = class_id.map(|c| c.as_u32());
    let loader = crate::classloader::loader_object_for_namespace_id_in(tables, ns)
        .or_else(|| class_owner.and_then(|c| crate::classloader::defining_loader_for(tables, c)));
    let loader = loader.and_then(|l| {
        crate::gc_stable_weak_lock_key(ctx, l)
            .ok()
            .map(|key| (l, key))
    });
    (loader, class_owner)
}

/// Insert, or upgrade in place, the [`defined_package_memo`] row for `key`.
///
/// A row that already holds `pkg` (a thin row `Class.getPackage()` enriched)
/// is promoted to `rich` and keeps the owner it has, filling in only what it
/// lacks. Any other row is replaced -- the last writer wins, as before w31-a.
pub fn publish_defined_package(
    tables: &crate::classloader::LoaderIdentityTables,
    key: DefinedPackageKey,
    pkg: ObjectRef,
    rich: bool,
    (loader, class_owner): (Option<(ObjectRef, usize)>, Option<u32>),
) {
    let mut memo = defined_package_memo(tables)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(row) = memo.get_mut(&key) {
        if row.pkg == pkg {
            row.rich |= rich;
            if row.loader.is_none() {
                row.loader = loader;
            }
            if row.class_owner.is_none() {
                row.class_owner = class_owner;
            }
            return;
        }
    }
    memo.insert(
        key,
        DefinedPackageRow {
            pkg,
            rich,
            loader,
            class_owner,
            in_loader_map: false,
        },
    );
}

/// Root scan for the `defined-packages` row of `vm/src/memory/native_roots.rs`
/// (gc-common w31-a): hand each of VM `vm`'s memoised `Package`s to `visit`
/// with its owner -- the owning user loader's address, or else the class it
/// was built for. The caller pins it to the loader under THIS VM's
/// loader-conditional licence (`metadata_pin`), or, with only a class owner,
/// through `defer_or_root`; a row with neither (a built-in namespace) is
/// rooted. The rows are copied out and the table lock dropped before the
/// first `visit`, so it never nests over `metadata_pin` / `loader_pin`.
pub fn gc_scan_defined_package_roots(
    tables: &crate::classloader::LoaderIdentityTables,
    visit: &mut dyn FnMut(Option<usize>, Option<u32>, ObjectRef),
) {
    let vm = tables.vm_identity();
    let rows: Vec<(Option<usize>, Option<u32>, ObjectRef)> = {
        let memo = defined_package_memo(tables)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if memo.is_empty() {
            return;
        }
        memo.iter()
            .filter(|((owner, _, _), _)| *owner == vm)
            .map(|(_, row)| {
                (
                    row.loader.map(|(l, _)| l.as_ptr() as usize),
                    row.class_owner,
                    row.pkg,
                )            })
            .collect()
    };
    for (loader, class_owner, pkg) in rows {
        visit(loader, class_owner, pkg);
    }
}

/// Post-move remap for [`gc_scan_defined_package_roots`]'s rows: VM `vm`'s
/// `Package`s and owning loaders the collection moved. A dead row (its loader
/// died) keeps its stale addresses until [`forget_lang_class_keys`] or
/// [`forget_unloaded_defined_packages`] drops it in the same epilogue;
/// nothing reads a row in between.
pub fn gc_update_defined_package_refs(
    tables: &crate::classloader::LoaderIdentityTables,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    let vm = tables.vm_identity();
    let moved = |obj: ObjectRef| -> Option<ObjectRef> {
        pointer_map.get(&(obj.as_ptr() as usize)).map(|&new_addr| {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: the collector's forwarding address for a live object.
            unsafe { ObjectRef::from_raw(new_addr as *mut u8) }
        })
    };
    let mut memo = defined_package_memo(tables)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for ((owner, _, _), row) in memo.iter_mut() {
        if *owner != vm {
            continue;
        }
        if let Some(new_pkg) = moved(row.pkg) {
            row.pkg = new_pkg;
        }
        if let Some((loader, key)) = row.loader {
            if let Some(new_loader) = moved(loader) {
                row.loader = Some((new_loader, key));
            }
        }
    }
}

/// gc-common w31-a: drop VM `vm`'s defined-`Package` rows built for the
/// classes the collection just UNLOADED (`lib.rs::forget_unloaded_proxy_classes`,
/// from `vm::memory::gc::unload_dead_class_metadata`'s unloading path). A class
/// unloads only with its loader, so the row's `Package` was pinned to a dead
/// loader and is gone; a row left behind would name a reclaimed object. The
/// loader's freed weak key drops the same rows ([`forget_lang_class_keys`]);
/// this catches a row whose loader could not be keyed. Answers the rows
/// dropped.
pub fn forget_unloaded_defined_packages(
    tables: &crate::classloader::LoaderIdentityTables,
    class_ids: &[u32],
) -> usize {
    if class_ids.is_empty() {
        return 0;
    }
    let vm = tables.vm_identity();
    let ids: rustc_hash::FxHashSet<u32> = class_ids.iter().copied().collect();
    let mut memo = defined_package_memo(tables)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let before = memo.len();
    memo.retain(|(owner, _, _), row| {
        *owner != vm || !row.class_owner.is_some_and(|c| ids.contains(&c))
    });
    before - memo.len()
}

/// Memo namespace for a **class**, on the same scale
/// `classloader::loader_namespace_id` uses for a **loader object**.
///
/// The two VM-side numberings differ at the low end and MUST be reconciled or
/// the two entry points key the same package differently and never share an
/// identity: `NativeContext::loader_id_of_class` reports
/// bootstrap/extension/application as `0`/`1`/`2`, whereas
/// `loader_namespace_id` collapses every built-in loader to `0` and only hands
/// out `allocate_loader_id` values (`>= 3`) for user-defined loaders — the very
/// pool `ClassLoaderId::UserDefined` draws from, so ids at that end already
/// agree.
///
/// gc-common w31: `loader_namespace_id` no longer collapses the platform and
/// application loaders -- since `dfbdab031` it answers `1` / `2` for them,
/// exactly `loader_id_of_class`'s numbering -- so the class's loader id IS the
/// namespace now, at every end. Folding `1` / `2` to `0` here (the old rule)
/// made `Foo.class.getPackage()` and the application loader's
/// `getDefinedPackage("..")` two different `Package` objects for every
/// classpath package (and likewise for the platform loader), in both modes;
/// HotSpot answers one. The other direction -- folding in
/// `getDefinedPackage` -- would make the application loader list the
/// bootstrap loader's packages. See
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub fn package_memo_ns_of_class(ctx: &dyn NativeContext, class_id: ClassId) -> u32 {
    ctx.loader_id_of_class(class_id) as u32
}

/// `loader.packages`, the field `java.lang.ClassLoader` DECLARES (the JDK's
/// `ConcurrentHashMap<String, NamedPackage>`), read through that class's slot.
///
/// t1 wave 9, lane INTV: every reader below used `get_field_by_name(loader,
/// "packages")`, which resolves the name from the RECEIVER's class upward, so a
/// user `ClassLoader` subclass with a field of its own named `packages` (a
/// common name in OSGi / plugin loaders) was read instead of the JDK's map:
/// its `Map.get` answered for `getDefinedPackage` / `Class.getPackage` /
/// `getDefinedPackages`, or a non-`Map` field made the `get` upcall fail and
/// the native threw. HotSpot runs the JDK body, whose `getfield
/// ClassLoader.packages` names the declaring class, so the subclass field is
/// never seen. A class library whose `ClassLoader` declares no `packages`
/// (the synthetic JDK) keeps the by-name read.
pub fn classloader_packages_field(ctx: &dyn NativeContext, loader: ObjectRef) -> Value {
    match ctx.resolve_field_index("java/lang/ClassLoader", "packages") {
        Some(index) => ctx.get_field(loader, index),
        None => ctx.get_field_by_name(loader, "packages"),
    }
}

/// The entry the receiver's OWN `packages` map holds for `package_name`, if
/// that map is readable and the entry is a real `java.lang.Package`.
///
/// **This is a reconciliation, not an optimisation.** `ClassLoader
/// .definePackage` runs real JDK bytecode against this exact map, and its
/// contract is `putIfAbsent(name, pkg) != null -> throw
/// IllegalArgumentException(name)`. A `getDefinedPackage` that cannot see the
/// map therefore contradicts the very call that populated it. Groovy's
/// `GroovyClassLoader.definePackageInternal` reads precisely that pair —
/// `if (getDefinedPackage(p) == null) definePackage(p, ...)` — so a
/// class-file-only answer kills the SECOND class defined in any one package
/// with `IllegalArgumentException: <pkg>`. Measured 2026-08-22: that is the
/// whole 11-class / ~70-method Spring Groovy cluster, and
/// `probes/DefinedPackageProbe.java` reproduces it with no Groovy present.
///
/// Every failure mode falls through to the classpath probe rather than
/// inventing an answer: a loader we were handed no reference for, an absent or
/// null `packages` field (the ByteBuddy `JavaDispatcher$DynamicClassLoader`
/// case this whole override exists for), a lookup that throws, or a stored
/// value that is not a `Package` (the JDK's lazy `NamedPackage` form, which the
/// module-taking `definePackage` overload is responsible for converting).
pub fn real_defined_package(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
    package_name: &str,
) -> Option<RealPackageEntry> {
    let loader = loader?;
    if !matches!(
        classloader_packages_field(ctx, loader),
        Value::Object(Some(_))
    ) {
        return None;
    }
    // The key first: `create_string` allocates (a GC point), and the map read
    // before it was used after it, unrooted (interpreter round i2 wave 55,
    // lane L5). The loader is pinned across the allocation and the map read
    // from its current address.
    let pin = ctx.pin_native_root(loader);
    let key = ctx.create_string(package_name);
    let loader = ctx.read_native_pin(pin, loader);
    ctx.unpin_native_roots(pin);
    let map = match classloader_packages_field(ctx, loader) {
        Value::Object(Some(map)) => map,
        _ => return None,
    };
    let got = ctx
        .invoke_virtual(
            map,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key))],
        )
        .ok()??;
    let entry = match got {
        Value::Object(Some(entry)) => entry,
        _ => return None,
    };
    // `Package extends NamedPackage`. The eight-arg `definePackage` stores the
    // full `Package`; defining a CLASS in a package stores only the lazy
    // `NamedPackage`. BOTH mean the package is defined — the difference is
    // whether we can hand back the JDK's own object or must supply one.
    Some(
        if ctx
            .class_name_of_id(ctx.class_id_of_object(entry))
            .is_some_and(|n| n == "java/lang/Package")
        {
            RealPackageEntry::Package(entry)
        } else {
            RealPackageEntry::Named(entry)
        },
    )
}

/// The application loader's `Package` for `pkg_name`, built the JDK's way,
/// for [`native_class_get_package`] of `mirror` (a class that loader
/// defined) under `CRATONVM_JDK_BUILT_PACKAGES` (interpreter round i2 wave 58,
/// lane L5; the i57-L5 proposal
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// HotSpot's `BuiltinClassLoader.defineClass(cn, Resource)` runs
/// `defineOrCheckPackage(pn, res.getManifest(), res.getCodeSourceURL())`
/// before it defines a class-path class, so the loader's `packages` map
/// holds a `Package` built by its private `definePackage(pn, man, url)` --
/// the per-entry section first, then the main section, read through the
/// JDK's own `Manifest`, and the code source as the seal base of a sealed
/// package -- and every door (`Class.getPackage`, `getDefinedPackage`,
/// `ClassLoader.packages()`) answers that object. The VM defines the class
/// itself, so this runs that `definePackage` the first time a door asks:
/// with the class's `file:` code source's `MANIFEST.MF` parsed by the JDK's
/// `Manifest(InputStream)`, or `null` for a directory entry (the JDK's
/// `Resource.getManifest()` there). The `Package` lands in the map (the
/// JDK body's `putIfAbsent`) and the memo, marked as in the map.
///
/// `None` (the caller's own path) for anything else: a `Package` already
/// handed out for the name (its identity is kept), the map's lazy
/// `NamedPackage`, a code source that is not a `file:` JAR or directory (the
/// VM's nested-jar and fat-jar spellings), or a JDK call that fails. Cold:
/// once per package.
#[cold]
#[inline(never)]
pub fn jdk_built_class_path_package(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    class_id: ClassId,
    pkg_name: &str,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let ns = cratonvm_types::ClassLoaderId::NATIVE_APPLICATION;
    let key = (vm, ns, pkg_name.to_string());
    if defined_package_memo(crate::classloader::loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
    {
        return None;
    }
    let (code_base, manifest_bytes) = class_path_entry_of(ctx, class_id)?;
    let loader = native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))]);
    let Ok(Some(Value::Object(Some(loader)))) = loader else {
        return None;
    };
    if !crate::classloader::loader_is_builtin(ctx, loader)
        || crate::classloader::loader_namespace_id(ctx, loader) != ns
    {
        return None;
    }
    // GC: every step below can run Java. The loader and each object made for
    // the call are pinned (stack order) and re-read before use; one unpin of
    // `loader_pin` releases them all.
    let loader_pin = ctx.pin_native_root(loader);
    let built = (|| -> Option<ObjectRef> {
        let loader = ctx.read_native_pin(loader_pin, loader);
        match real_defined_package(ctx, Some(loader), pkg_name) {
            Some(RealPackageEntry::Package(pkg)) => return Some(pkg),
            Some(RealPackageEntry::Named(_)) => return None,
            None => {}
        }
        let (manifest, url) = jdk_manifest_and_url(
            ctx,
            manifest_bytes.as_deref().map(|b| b.as_slice()),
            &code_base,
        )?;
        let name = ctx.create_string(pkg_name);
        let manifest = manifest.map(|(pin, m)| ctx.read_native_pin(pin, m));
        let url = url.map(|(pin, u)| ctx.read_native_pin(pin, u));
        let loader = ctx.read_native_pin(loader_pin, loader);
        let ran = ctx.invoke_special(
            "jdk/internal/loader/BuiltinClassLoader",
            "definePackage",
            "(Ljava/lang/String;Ljava/util/jar/Manifest;Ljava/net/URL;)Ljava/lang/Package;",
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(name)),
                Value::Object(manifest),
                Value::Object(url),
            ],
        );
        match ran {
            Ok(Some(Value::Object(Some(pkg)))) => Some(pkg),
            // Another thread defined it first (the JDK body's
            // `IllegalArgumentException`): answer the map's, as the JDK's
            // `defineOrCheckPackage` re-reads it.
            _ => {
                let loader = ctx.read_native_pin(loader_pin, loader);
                match real_defined_package(ctx, Some(loader), pkg_name) {
                    Some(RealPackageEntry::Package(pkg)) => Some(pkg),
                    _ => None,
                }
            }
        }
    })();
    ctx.unpin_native_roots(loader_pin);
    let pkg = built?;
    if false {
        eprintln!(
            "[builtin-loader-packages] jdk-built {pkg_name:?} manifest={}",
            manifest_bytes.is_some()
        );
    }
    // No GC point from the last Java call to the insert.
    let tables = crate::classloader::loader_tables(ctx);
    publish_defined_package(tables, key.clone(), pkg, true, (None, None));
    if let Some(row) = defined_package_memo(crate::classloader::loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&key)
    {
        row.in_loader_map = true;
    }
    Some(pkg)
}

/// The code source of `class_id` when it is a `file:` class-path entry, with
/// that JAR's `META-INF/MANIFEST.MF` bytes (`None` for a directory entry, as
/// the JDK's `Resource.getManifest()` answers there). `None` for any other
/// code source (the VM's nested-jar and fat-jar spellings, a missing path).
pub fn class_path_entry_of(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Option<(String, Option<std::sync::Arc<Vec<u8>>>)> {
    let code_base = ctx.class_code_base(class_id)?;
    let trimmed = code_base.strip_prefix("file:")?.trim_start_matches('/');
    let path = if cfg!(windows) {
        std::path::PathBuf::from(trimmed)
    } else {
        std::path::PathBuf::from(format!("/{trimmed}"))
    };
    let manifest_bytes = if path.is_file() {
        crate::phases_late::jar_manifest::jar_entry_bytes_cached(
            &path.to_string_lossy(),
            "META-INF/MANIFEST.MF",
        )    } else if path.is_dir() {
        None
    } else {
        return None;
    };
    Some((code_base, manifest_bytes))}

/// This VM's parsed class-path `Manifest`s for [`jdk_manifest_and_url`]: code source URL text ->
/// global-root handle of the JDK's `java.util.jar.Manifest` (one per code source, as HotSpot's
/// `JarFile` keeps one). Per VM (JIT round 21, lane manifest,
/// `jr21-manifest-cache-rows-outlive-their-vm-20261014.md`): it was a process-global map keyed
/// `(vm_identity, code source)` that no teardown named, so every VM left its rows behind.
/// [`forget_vm_lang_class_tables`] drops the VM's row at teardown; the handles die with that VM's
/// global-root table, so nothing is released there.
pub static JDK_CLASS_PATH_MANIFESTS: VmScoped<FxHashMap<String, usize>> = VmScoped::new();

/// The JDK's `Manifest` of `manifest_bytes` (through `Manifest(InputStream)`;
/// `None` for no bytes) and the JDK's `URL` of `code_base` (`None` when it
/// cannot be made), each pinned as `(pin, object)`; the caller re-reads them
/// and releases the pins from a lower pin of its own. `None` when the
/// `Manifest` cannot be built. Runs Java.
#[allow(clippy::type_complexity)]
pub fn jdk_manifest_and_url(
    ctx: &mut dyn NativeContext,
    manifest_bytes: Option<&[u8]>,
    code_base: &str,
) -> Option<(Option<(usize, ObjectRef)>, Option<(usize, ObjectRef)>)> {
    // HotSpot parses a JAR's manifest ONCE (`JarFile.getManifest` keeps it); this door asked once per
    // PACKAGE, so a signed jar's 5,000-section manifest was parsed again for every package of a class
    // path library (BouncyCastle's 65 packages: ~85 % of a cold provider initialisation, `--nojit` 30 s
    // against HotSpot's 1 s `-Xint`). One `Manifest` per (VM, code source), held by a global root: the
    // package's `definePackage` only reads its attributes. The rows are this VM's
    // ([`JDK_CLASS_PATH_MANIFESTS`], dropped with it by `forget_vm_lang_class_tables`).
    let vm = ctx.vm_identity();
    // `peek`, then resolve outside the table lock (`VmScoped` lock discipline).
    let row_handle = JDK_CLASS_PATH_MANIFESTS
        .peek(vm, |rows| rows.get(code_base).copied())
        .flatten();
    let cached = row_handle.and_then(|handle| ctx.resolve_global_root(handle));
    let manifest = match (manifest_bytes, cached) {
        (Some(_), Some(man)) => Some((ctx.pin_native_root(man), man)),
        (Some(bytes), None) => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            if !ctx.write_byte_array_from(arr, 0, bytes) {
                return None;
            }
            let arr_pin = ctx.pin_native_root(arr);
            let arr = ctx.read_native_pin(arr_pin, arr);
            let Ok(Some(Value::Object(Some(bais)))) = ctx.new_object_initialized(
                "java/io/ByteArrayInputStream",
                "([B)V",
                &[Value::Object(Some(arr))],
            ) else {
                return None;
            };
            let Ok(Some(Value::Object(Some(man)))) = ctx.new_object_initialized(
                "java/util/jar/Manifest",
                "(Ljava/io/InputStream;)V",
                &[Value::Object(Some(bais))],
            ) else {
                return None;
            };
            let man_pin = ctx.pin_native_root(man);
            let man = ctx.read_native_pin(man_pin, man);
            let handle = ctx.add_global_root(man);
            if handle != 0 {
                // Two threads defining the first packages of one jar can both miss and both parse:
                // keep the first row and release this thread's root, which no row would ever name
                // (it would otherwise pin a second `Manifest` for the VM's life). Released after
                // the table lock is dropped. A row whose handle no longer resolved (the miss that
                // brought us here) is replaced, or every later package would parse again.
                let kept = JDK_CLASS_PATH_MANIFESTS.with(vm, |rows| {
                    let slot = rows.entry(code_base.to_string()).or_insert(handle);
                    if row_handle == Some(*slot) {
                        *slot = handle;
                    }
                    *slot
                });
                if kept != handle {
                    ctx.remove_global_root(handle);
                }
            }
            Some((man_pin, man))
        }
        (None, _) => None,
    };
    let url = match new_jdk_url(ctx, code_base) {
        Ok(Some(url)) => Some((ctx.pin_native_root(url), url)),
        _ => None,
    };
    Some((manifest, url))}

/// HotSpot's `BuiltinClassLoader.defineClass(cn, Resource)` step for a class
/// the VM defined itself: `defineOrCheckPackage(pn, man, url)` for the
/// application loader's class `class_id` from a `file:` class-path entry,
/// with the entry's `Manifest` (the JDK's, `null` for a directory) and its
/// code source URL. The JDK's body defines the package on its first class
/// (`definePackage(pn, man, url)`, into the loader's `packages` map) and, for
/// every later class, checks its sealing (`getAndVerifyPackage`): a class of a
/// sealed package from another code source, or a sealing JAR's class of a
/// package already defined unsealed, is a `SecurityException` ("sealing
/// violation: ..."), which this returns for the caller to fail the class
/// with. Interpreter round i2 wave 62, lane L5, the next stage of
/// `docs/feature-designs/class-resolution-and-caching.md`:
/// the VM calls it when it LINKS the class (`vm_util::link_claimed_class`),
/// its first point that may run Java, behind
/// `CRATONVM_CLASS_PATH_PACKAGE_AT_LINK`.
///
/// `Ok(true)` when the JDK's body ran and accepted the class, `Ok(false)` when
/// it did not apply (another loader, the unnamed package, a code source that
/// is not a `file:` JAR or directory, a package whose `Package` was handed out
/// outside the loader's map -- its identity is kept -- or a JDK call that
/// failed other than with a `SecurityException`). Cold: the caller asks once
/// per (package, code source).
#[cold]
#[inline(never)]
pub fn define_or_check_class_path_package(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Result<bool, MethodCallFailed> {
    let Some(name) = ctx.class_name_of_id(class_id) else {
        return Ok(false);
    };
    // `BuiltinClassLoader.defineClass` defines no package for a class of the
    // unnamed package (`pos != -1`).
    let Some((pkg_slash, _)) = name.rsplit_once('/') else {
        return Ok(false);
    };
    // A named module's package (the module path) is defined with its module
    // (`BuiltinClassLoader.defineClass(cn, LoadedModule)` runs no
    // `defineOrCheckPackage`); only the class path's unnamed module is this
    // step's (a modular JAR on the class path is in it).
    if ctx
        .module_for_package(pkg_slash)
        .is_some_and(|module| !ctx.module_is_class_path_only(&module))
    {
        return Ok(false);
    }
    let pkg_name = pkg_slash.replace('/', ".");
    let Some((code_base, manifest_bytes)) = class_path_entry_of(ctx, class_id) else {
        return Ok(false);
    };
    let ns = cratonvm_types::ClassLoaderId::NATIVE_APPLICATION;
    let key = (ctx.vm_identity(), ns, pkg_name.clone());
    let row_in_map = defined_package_memo(crate::classloader::loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|row| row.in_loader_map);
    if row_in_map == Some(false) {
        return Ok(false);
    }
    let mirror = ctx.get_class_mirror(class_id);
    let Ok(Some(Value::Object(Some(loader)))) =
        native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))])
    else {
        return Ok(false);
    };
    if !crate::classloader::loader_is_builtin(ctx, loader)
        || crate::classloader::loader_namespace_id(ctx, loader) != ns
    {
        return Ok(false);
    }
    // GC: every step below can run Java. The loader and each object made for
    // the call are pinned (stack order) and re-read before use; one unpin of
    // `loader_pin` releases them all.
    let loader_pin = ctx.pin_native_root(loader);
    let ran = (|| -> Result<Option<ObjectRef>, MethodCallFailed> {
        let Some((manifest, url)) = jdk_manifest_and_url(
            ctx,
            manifest_bytes.as_deref().map(|b| b.as_slice()),
            &code_base,
        ) else {
            return Ok(None);
        };
        // Not interned: the JDK's `cn.substring(0, pos)`.
        let pn = ctx.create_string_uninterned(&pkg_name);
        let manifest = manifest.map(|(pin, m)| ctx.read_native_pin(pin, m));
        let url = url.map(|(pin, u)| ctx.read_native_pin(pin, u));
        let loader = ctx.read_native_pin(loader_pin, loader);
        match ctx.invoke_special(
            "jdk/internal/loader/BuiltinClassLoader",
            "defineOrCheckPackage",
            "(Ljava/lang/String;Ljava/util/jar/Manifest;Ljava/net/URL;)Ljava/lang/Package;",
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(pn)),
                Value::Object(manifest),
                Value::Object(url),
            ],
        ) {
            Ok(Some(Value::Object(Some(pkg)))) => Ok(Some(pkg)),
            Ok(_) => Ok(None),
            Err(MethodCallFailed::ExceptionThrown(exc))
                if ctx
                    .class_name_of_id(ctx.class_id_of_object(exc))
                    .is_some_and(|n| n == "java/lang/SecurityException") =>
            {
                Err(MethodCallFailed::ExceptionThrown(exc))
            }
            Err(_) => Ok(None),
        }
    })();
    ctx.unpin_native_roots(loader_pin);
    let dbg = false;
    let pkg = match ran {
        Ok(Some(pkg)) => pkg,
        Ok(None) => return Ok(false),
        Err(e) => {
            if dbg {
                eprintln!("[builtin-loader-packages] at-link {pkg_name:?} sealing violation");            }
            return Err(e);
        }
    };
    if dbg {
        eprintln!(
            "[builtin-loader-packages] at-link {pkg_name:?} manifest={}",
            manifest_bytes.is_some()
        );
    }
    // The package's first class: the memo takes the map's `Package` (no GC
    // point from the Java call's return to the insert).
    if row_in_map.is_none() {
        let tables = crate::classloader::loader_tables(ctx);
        publish_defined_package(tables, key.clone(), pkg, true, (None, None));
        if let Some(row) = defined_package_memo(crate::classloader::loader_tables(ctx))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&key)
        {
            row.in_loader_map = true;
        }
    }
    Ok(true)
}

/// Put a `Package` for every package of a class the VM defined for the
/// built-in loader `loader` (the platform or the application loader) into that
/// loader's own `packages` map, where HotSpot's loaders have one: the JDK's
/// listings (`ClassLoader.packages()`, hence `getDefinedPackages()` and an
/// unnamed module's `Module.getPackages()`) read only that map, and the VM
/// defines a class-path or image class without running the JDK's
/// `BuiltinClassLoader.defineClass` that writes it. The `Package` is the
/// memo's ([`native_class_get_package`] of one class of the package), so
/// `Class.getPackage()`, `getDefinedPackage(name)` and the map agree on one
/// object; an entry the map already holds is left alone. Measured on HotSpot
/// 25.0.3 (`tools/probes/interp/L5/L5W55UnnamedModulePackages.java`): the
/// application loader lists the unnamed package of the main class from the
/// start, and the platform loader lists `java.sql` once `java.sql.Date` is
/// loaded, with no `getPackage` call in between.
///
/// A package already synced (its memo row says so) costs no Java call; a new
/// one costs a `get` and a `putIfAbsent` on the map. No-op for any other
/// loader, before the built-in loaders are adopted, or with
/// `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC=0`. Interpreter round i2 wave 55,
/// lane L5.
pub fn sync_builtin_loader_packages(ctx: &mut dyn NativeContext, loader: ObjectRef) {
    let vm = ctx.vm_identity();
    if !ctx.builtin_loader_package_sync() || !crate::classloader::loader_is_builtin(ctx, loader) {
        return;
    }
    let ns = crate::classloader::loader_namespace_id(ctx, loader);
    let platform = match ns {
        cratonvm_types::ClassLoaderId::NATIVE_APPLICATION => false,
        cratonvm_types::ClassLoaderId::NATIVE_EXTENSION => true,
        _ => return,
    };
    // The loader's own tag, and the boot tag (0) that an image class of a
    // module the JDK's table defines to this loader carries.
    let mut reps: Vec<(u32, String, ClassId)> = Vec::new();
    for tag in [ns, 0] {
        reps.extend(
            ctx.loaded_package_representatives(tag)
                .into_iter()
                .map(|(pkg, cid)| (tag, pkg, cid)),
        );
    }
    let mut pending: Vec<(String, ClassId)> = Vec::new();
    for (tag, pkg_slash, cid) in reps {
        let dotted = pkg_slash.replace('/', ".");
        let synced = defined_package_memo(crate::classloader::loader_tables(ctx))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(vm, ns, dotted.clone()))
            .is_some_and(|row| row.in_loader_map);
        if synced
            || (tag == 0
                && (pkg_slash.is_empty()
                    || !crate::classloader::image_package_defined_to(ctx, &pkg_slash, platform)))
        {
            continue;
        }
        pending.push((dotted, cid));
    }
    if pending.is_empty() {
        return;
    }
    if false {
        eprintln!(
            "[builtin-loader-packages] ns={ns}: {} package(s) to sync",
            pending.len()
        );
    }
    // GC: every step below can run Java. The loader is pinned and re-read
    // before each use; each `Package` is pinned across the key's allocation.
    let loader_pin = ctx.pin_native_root(loader);
    for (name, cid) in pending {
        let loader = ctx.read_native_pin(loader_pin, loader);
        let present = real_defined_package(ctx, Some(loader), &name).is_some();
        if !present {
            let mirror = ctx.get_class_mirror(cid);
            let Ok(Some(Value::Object(Some(pkg)))) =
                native_class_get_package(ctx, &[Value::Object(Some(mirror))])
            else {
                continue;
            };
            let pkg_pin = ctx.pin_native_root(pkg);
            let key = ctx.create_string(&name);
            let pkg = ctx.read_native_pin(pkg_pin, pkg);
            ctx.unpin_native_roots(pkg_pin);
            let loader = ctx.read_native_pin(loader_pin, loader);
            let Value::Object(Some(map)) = ctx.get_field_by_name(loader, "packages") else {
                break;
            };
            if ctx
                .invoke_virtual(
                    map,
                    "putIfAbsent",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(key)), Value::Object(Some(pkg))],
                )
                .is_err()
            {
                continue;
            }
        }
        if let Some(row) = defined_package_memo(crate::classloader::loader_tables(ctx))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(vm, ns, name))
        {
            row.in_loader_map = true;
        }
    }
    ctx.unpin_native_roots(loader_pin);
}

