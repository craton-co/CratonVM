#[allow(unused_imports)]
use super::*;

/// Parse the argument types from a method descriptor.
/// Returns `(total formal param count, is the LAST formal param exactly
/// `Ljava/lang/Object;[]`)` for a method descriptor -- used by
/// `bootstrap_generic` to detect the "trailing `Object[]` collects extra
/// bootstrap args" invokedynamic linkage shape (JVMS-legal; mirrors a Java
/// varargs bootstrap method). Deliberately narrower than a general varargs
/// check: per JVMS this collecting parameter is always both syntactically
/// LAST and always exactly `Object[]` (never some other array component
/// type) for a bootstrap method, so no Block-after-array-style ordering
/// complication applies here the way it did for JRuby's own MethodHandle
/// combinator chains (see `collect_trailing_varargs` in
/// `native-builtins/src/lang_invoke.rs` for that unrelated, harder case).
pub(in crate::runtime::invokedynamic) fn descriptor_param_count_and_last_is_object_array(descriptor: &str) -> Option<(usize, bool)> {
    let inner = descriptor.strip_prefix('(')?;
    let close = inner.find(')')?;
    let params_str = &inner[..close];
    let bytes = params_str.as_bytes();
    let mut i = 0;
    let mut count = 0usize;
    let mut last_is_object_array = false;
    while i < bytes.len() {
        let start = i;
        last_is_object_array = false;
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
                let token = &params_str[start..i];
                last_is_object_array = token == "[Ljava/lang/Object;";
            }
            _ => {
                i += 1;
            }
        }
        count += 1;
    }
    Some((count, last_is_object_array))
}

/// The component class of a bootstrap's LAST parameter when it is a
/// one-dimensional array of a reference type other than `Object`
/// (`[Ljava/lang/invoke/MethodHandle;` -> `java/lang/invoke/MethodHandle`),
/// else `None`. The narrow comment above
/// [`descriptor_param_count_and_last_is_object_array`] ("always exactly
/// `Object[]`") does not hold: `ObjectMethods.bootstrap` ends in
/// `MethodHandle...` (interpreter round i1 wave 40, lane L4).
pub(in crate::runtime::invokedynamic) fn trailing_typed_array_component(descriptor: &str) -> Option<&str> {
    let inner = descriptor.strip_prefix('(')?;
    let params = &inner[..inner.find(')')?];
    let bytes = params.as_bytes();
    let mut i = 0;
    let mut last: Option<&str> = None;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        last = params.get(start..i.min(bytes.len()));
    }
    let component = last?.strip_prefix("[L")?.strip_suffix(';')?;
    (component != "java/lang/Object").then_some(component)
}

pub(in crate::runtime::invokedynamic) fn parse_descriptor_args(descriptor: &str) -> Vec<char> {
    let mut args = Vec::new();
    parse_descriptor_args_into(descriptor, &mut args);
    args
}

/// [`parse_descriptor_args`] into caller-provided storage, so a hot caller can
/// hand it a `SmallVec` and allocate nothing. One tag per parameter: the
/// primitive's own letter, or `L` for any reference (arrays included).
pub(in crate::runtime::invokedynamic) fn parse_descriptor_args_into<E: Extend<char>>(descriptor: &str, args: &mut E) {
    let bytes = descriptor.as_bytes();
    let mut i = 0;
    let mut push = |c: char| args.extend(std::iter::once(c));

    if i < bytes.len() && bytes[i] == b'(' {
        i += 1;
    }

    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'I' | b'S' | b'Z' => {
                push(bytes[i] as char);
                i += 1;
            }
            b'J' => {
                push('J');
                i += 1;
            }
            b'F' => {
                push('F');
                i += 1;
            }
            b'D' => {
                push('D');
                i += 1;
            }
            b'L' => {
                push('L');
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                push('L');
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
}

/// Convert a JVM Value to its string representation for string concatenation.
///
/// When `thread` is provided, objects that are not strings or primitive wrappers
/// will have their `toString()` called via virtual dispatch. This produces
/// correct output for ArrayList, HashMap, user classes, etc.
///
/// `Err` only for a `toString()` that completed abruptly with a Java-visible
/// throwable, which the caller must propagate: HotSpot's
/// `StringConcatHelper.stringOf` does not catch it. Every other failure keeps
/// the historical `ClassName@hash` fallback.
///
/// Lossy for an unpaired surrogate in a `toString()` result (a Rust `String`
/// cannot hold one); the concat path uses [`render_operand_checked`] instead.
pub(in crate::runtime::invokedynamic) fn value_to_string_checked(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> Result<String, MethodCallFailed> {
    Ok(
        match render_operand_checked(shared, thread, value, type_char)? {
            RenderedOperand::Text(s) => s,
            RenderedOperand::Units(units) => String::from_utf16_lossy(&units),
        },
    )
}

/// One rendered concat operand: Rust text for everything VM-formatted
/// (primitives, wrappers, the `ClassName@hash` fallback), UTF-16 code units for
/// a Java `String` — including the one a `toString()` returned, which may hold
/// an unpaired surrogate that a Rust `String` would turn into U+FFFD.
pub(in crate::runtime::invokedynamic) enum RenderedOperand {
    Text(String),
    Units(Vec<u16>),
}

/// [`value_to_string_checked`] without the lossy last step.
///
/// gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
/// the object arm runs `toString()`, so an allocation in it cannot be retried;
/// it takes the allocators' unwinding arm instead of the infallible one (which
/// aborts on Generational/ZGC and takes G1's reserve), and a heap-exhaustion
/// unwind becomes this operand's `OutOfMemoryError`.
pub(in crate::runtime::invokedynamic) fn render_operand_checked(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> Result<RenderedOperand, MethodCallFailed> {
    match crate::runtime::native_oom::catch_alloc_oom(|| {
        render_operand_checked_inner(shared, thread, value, type_char)
    }) {
        Ok(r) => r,
        Err(oom) => Err(MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        )),
    }
}

pub(in crate::runtime::invokedynamic) fn render_operand_checked_inner(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> Result<RenderedOperand, MethodCallFailed> {
    Ok(RenderedOperand::Text(match value {
        Value::Int(v) => match type_char {
            'Z' => {
                if *v != 0 {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            'C' => {
                if let Some(ch) = char::from_u32(*v as u32) {
                    ch.to_string()
                } else {
                    format!("\\u{:04x}", *v as u16)
                }
            }
            _ => v.to_string(),
        },
        Value::Long(v) => v.to_string(),
        Value::Float(v) => format_float(*v),
        Value::Double(v) => format_double(*v),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(obj_ref)) => {
            // A Java String first -- by class, not by shape (wave 16): the
            // shape reader answers for a `StringBuilder` too, with its whole
            // buffer. See `vm::is_java_lang_string`.
            if crate::vm::is_java_lang_string(shared, *obj_ref) {
                if let Some(units) = read_java_string_units(&shared.mem.heap, *obj_ref) {
                    return Ok(RenderedOperand::Units(units));
                }
            }

            // Primitive wrappers render without a Java call. Arrays must NOT
            // take this path: num_slots is the array LENGTH, so a length-1
            // array would masquerade as a wrapper and packed primitive arrays
            // would read a garbage Value slot.
            //
            // Keyed on the EXACT wrapper class, not on "one slot holding a
            // primitive". The shape test alone answered for every user class
            // with a single primitive field -- `"" + new Counter(5)` printed
            // `5` without ever running `Counter.toString()` -- and its
            // `contains("Boolean")` test misrendered any such class whose name
            // merely contained the word. The wrappers are final, so the exact
            // name is exact.
            let is_array =
                shared.mem.heap.kind_of(*obj_ref) == crate::memory::heap::ObjectKind::Array;
            let nf = shared.mem.heap.get_header(*obj_ref).num_slots() as usize;
            if nf == 1 && !is_array {
                let class_id = shared.mem.heap.class_id_of(*obj_ref);
                let name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.clone());
                let field = shared.mem.heap.get_field(*obj_ref, 0);
                if let Some(s) = name.and_then(|n| primitive_wrapper_display(&n, field)) {
                    return Ok(RenderedOperand::Text(s));
                }
            }

            // Call toString() via virtual dispatch if thread context is available
            if let Some(t) = thread {
                let mut ctx = NativeContextImpl { shared, thread: t };
                use cratonvm_native_api::NativeContext;

                // `java.nio.file.Path` is a genuine interface with no `toString()`
                // body of its own; `ctx.invoke_virtual` below doesn't consult
                // `force_native_over_real_jdk_bytecode`/the `vm_exec.rs`
                // `check_override` allow-list the way the bytecode interpreter's
                // own `invokevirtual` handling does, so it silently resolves to
                // `Object.toString()` here too (`java.nio.file.Path@<hash>`) for
                // `"literal" + aPath` string concatenation. Same family as
                // `path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`,
                // a third, distinct call site. Route through the same
                // display-string helper the registered `Path.toString()` native
                // itself uses, bypassing `invoke_virtual` entirely for this type.
                //
                // NOTE: must use the ClassId-based `is_subclass_of` (which walks
                // both the superclass chain AND implemented interfaces), not
                // `ClassManager::is_subclass_of_by_name` — that one is the
                // exception-`catch_type` fallback and deliberately walks ONLY
                // the superclass chain (interfaces are never a `catch_type`),
                // so it can never match an interface like `Path` and this
                // branch would silently never fire. (A concurrent dev commit
                // added this same check using `is_subclass_of_by_name` — that
                // version never actually fires; confirmed via a standalone
                // `"file:" + Paths.get(...)` repro that still printed
                // `file:java.nio.file.Path@<hash>` until switched to
                // `is_subclass_of`.)
                //
                // Compatible mode only (wave 4). The helper is the same body
                // as the `Path.toString` Bridge that `register_phase57_nio_file`
                // registers on `java/nio/file/Path` and mirrors onto the
                // concrete `WindowsPath`/`UnixPath` the minted paths carry; it
                // is load-bearing there because a jar/jrt path is such an
                // object whose `path` slot holds the sentinel encoding. Under
                // `--real-jdk` real class bytes are authoritative: the path is
                // a genuine JDK object (a real `ZipPath` has its own layout,
                // which the slot-reading helper does not know), and
                // `invoke_virtual` below applies the admitted dispatch.
                let obj_class_id = shared.mem.heap.class_id_of(*obj_ref);
                let is_path = !crate::vm::dispatch_policy(shared).is_jdk_only()
                    && ctx
                        .class_id_by_name("java/nio/file/Path")
                        .is_some_and(|path_cid| {
                            shared
                                .classes
                                .class_manager
                                .read()
                                .is_subclass_of(obj_class_id, path_cid)
                        });
                if is_path {
                    return Ok(RenderedOperand::Text(
                        cratonvm_native_builtins::phases_late::p57_path_display_string(
                            &mut ctx, *obj_ref,
                        ),
                    ));
                }

                match ctx.invoke_virtual(*obj_ref, "toString", "()Ljava/lang/String;", &[]) {
                    // The result is read as code units: `ctx.read_string` went
                    // UTF-16 -> Rust `String` -> UTF-16 and turned an unpaired
                    // surrogate into U+FFFD, the last lossy leg of `+`.
                    Ok(Some(Value::Object(Some(str_ref)))) => {
                        if let Some(units) = read_java_string_units(&shared.mem.heap, str_ref) {
                            return Ok(RenderedOperand::Units(units));
                        }
                        return Ok(RenderedOperand::Text(
                            ctx.read_string(str_ref)
                                .unwrap_or_else(|| "null".to_string()),
                        ));
                    }
                    // `toString()` returned null, which is legal: HotSpot's
                    // `StringConcatHelper.stringOf` renders it as "null". This
                    // used to fall through to the `ClassName@hash` fallback.
                    Ok(Some(Value::Object(None))) => {
                        return Ok(RenderedOperand::Text("null".to_string()))
                    }
                    // A throwing `toString()` propagates. It used to be
                    // swallowed into `ClassName@hash`.
                    Err(e @ MethodCallFailed::ExceptionThrown(_))
                    | Err(e @ MethodCallFailed::InternalError(VmError::Runtime(_))) => {
                        return Err(e);
                    }
                    _ => {}
                }
            }

            // Final fallback: ClassName@hash. For arrays the header's class_id
            // is the COMPONENT class — render the JVMS array-class name
            // ([Ljava.lang.Class; / [I) like HotSpot instead.
            let class_name = if is_array {
                crate::runtime::interpreter::array_descriptor_of(shared, *obj_ref)
                    .unwrap_or_else(|| "[Ljava/lang/Object;".to_string())
            } else {
                let class_id = shared.mem.heap.class_id_of(*obj_ref);
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| "?".to_string())
            };
            let dotted = crate::runtime::exceptions::hotspot_external_name(&class_name);
            // JIT round 21, lane hashlock (wave 3, tag `hl21`): the Java-visible
            // identity hash, as `Object.toString()` prints `hashCode()` -- the
            // heap's own answer is 0 for a locked compact instance whose word
            // holds no hash (this fallback then printed `@0`, and a later
            // `hashCode()` minted a different value).
            let hash = crate::runtime::interpreter::vm_identity_hash(shared, *obj_ref);
            format!("{dotted}@{hash:x}")
        }
        _ => "?".to_string(),
    }))
}

/// `toString()` of a primitive wrapper, from its exact class name and its one
/// field -- `None` for any other class. Exact names, because the wrappers are
/// final: see the caller for the shape test this replaced.
pub(in crate::runtime::invokedynamic) fn primitive_wrapper_display(class_name: &str, field: Value) -> Option<String> {
    Some(match (class_name, field) {
        ("java/lang/Boolean", Value::Int(v)) => String::from(if v != 0 { "true" } else { "false" }),
        ("java/lang/Character", Value::Int(v)) => {
            char::from_u32(v as u32).unwrap_or('?').to_string()
        }
        ("java/lang/Integer" | "java/lang/Short" | "java/lang/Byte", Value::Int(v)) => {
            v.to_string()
        }
        ("java/lang/Long", Value::Long(v)) => v.to_string(),
        ("java/lang/Float", Value::Float(v)) => format_float(v),
        ("java/lang/Double", Value::Double(v)) => format_double(v),
        _ => return None,
    })
}

/// Format a float value like Java's `Float.toString` (string-concat path).
/// Delegates to the shared `cratonvm_types` formatter so the JLS layout —
/// including the 10^-3..10^7 scientific-notation threshold — stays consistent
/// with `Double.toString`/`StringBuilder.append`. The old local `format!("{v}")`
/// never used E-notation, so `1e8f` concatenated as "100000000.0" not "1.0E8".
pub(crate) fn format_float(v: f32) -> String {
    cratonvm_types::java_float_to_string(v)
}

/// Format a double value like Java's `Double.toString` (string-concat path).
/// See `format_float`.
pub(crate) fn format_double(v: f64) -> String {
    cratonvm_types::java_double_to_string(v)
}

// ---------------------------------------------------------------------------
// SwitchBootstraps — JEP 441 (Pattern Matching for switch, Java 21)
// ---------------------------------------------------------------------------

/// Bootstrap a `SwitchBootstraps.typeSwitch` call site.
///
/// Bootstrap arguments are an array of labels: each is a `Class<?>` (type check),
/// an `Integer` (exact int match), or a `String` (exact string match).
/// ClassIds are pre-resolved here so the execution loop needs no locks.
pub(in crate::runtime::invokedynamic) fn bootstrap_type_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Phase 1: extract label names from constant pool (read lock only).
    let raw_labels: Vec<RawSwitchLabel> = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("typeSwitch: class {current_class_id} not found"),
            })?;

        let mut raw = Vec::with_capacity(info.bootstrap_arg_indices.len());
        for &arg_idx in &info.bootstrap_arg_indices {
            match class.constant_pool.get(arg_idx) {
                Some(ConstantPoolEntry::ClassReference { name_index }) => {
                    let name = class
                        .constant_pool
                        .get_utf8(*name_index)
                        .unwrap_or("")
                        .to_string();
                    raw.push(RawSwitchLabel::Type(name));
                }
                Some(ConstantPoolEntry::Integer(v)) => {
                    raw.push(RawSwitchLabel::Int(*v));
                }
                Some(ConstantPoolEntry::Long(v)) => {
                    raw.push(RawSwitchLabel::Long(v.get()));
                }
                Some(ConstantPoolEntry::Float(v)) => {
                    raw.push(RawSwitchLabel::Float(*v));
                }
                Some(ConstantPoolEntry::Double(v)) => {
                    raw.push(RawSwitchLabel::Double(v.get()));
                }
                Some(ConstantPoolEntry::StringReference { string_index }) => {
                    let s = class
                        .constant_pool
                        .get_utf8(*string_index)
                        .unwrap_or("")
                        .to_string();
                    raw.push(RawSwitchLabel::Str(s));
                }
                Some(ConstantPoolEntry::Dynamic {
                    name_and_type_index,
                    ..
                }) => {
                    // JDK 25 primitive patterns (JEP 507): Dynamic constant that
                    // resolves via ConstantBootstraps.primitiveClass to a primitive
                    // Class (int.class, long.class, etc.). The name in the
                    // NameAndType is the primitive type descriptor ("I", "J", etc.).
                    //
                    // An entry without a usable NameAndType still takes its
                    // position (as a label that matches nothing): the label
                    // INDEX is what the switch's tableswitch branches on, so
                    // dropping one shifted every later case onto its
                    // neighbour's arm.
                    //
                    // A condy of type `EnumDesc` is a qualified enum constant
                    // label (`case E.A`, JEP 441), not a primitive class. It
                    // used to be read as `PrimitiveClass("invoke")`, which
                    // matched nothing, so `E.A` fell to a later `case E e`.
                    let (name, type_desc) = class
                        .constant_pool
                        .get_name_and_type(*name_and_type_index)
                        .unwrap_or(("", ""));
                    if type_desc == "Ljava/lang/Enum$EnumDesc;" {
                        raw.push(
                            match javac_enum_desc_label(
                                &class.constant_pool,
                                &class.bootstrap_methods,
                                arg_idx,
                            ) {
                                Some((class_name, constant)) => RawSwitchLabel::EnumDesc {
                                    class_name,
                                    constant,
                                },
                                None => RawSwitchLabel::PrimitiveClass(String::new()),
                            },
                        );
                    } else {
                        raw.push(RawSwitchLabel::PrimitiveClass(name.to_string()));
                    }
                }
                _ => {
                    tracing::warn!(
                        "Unrecognized constant pool entry type in switch label resolution (index {})",
                        arg_idx
                    );
                    // Keep the position (see above): an empty descriptor is a
                    // label no target matches.
                    raw.push(RawSwitchLabel::PrimitiveClass(String::new()));
                }
            }
        }
        raw
    }; // read lock dropped

    // Phase 2: resolve all Type labels to ClassIds (one write lock per class, done once).
    let mut labels = Vec::with_capacity(raw_labels.len());
    for raw in raw_labels {
        match raw {
            RawSwitchLabel::Type(name) => {
                // A `case Foo f` label is a `CONSTANT_Class` of the switching
                // class, so it resolves through THAT class's defining loader,
                // exactly as a `checkcast Foo` in the same method would. The
                // loader-blind `load_class_concurrent` asked only the built-in
                // chain: a pattern switch in a webapp / fat-jar / forked-test
                // class over one of its own types failed to link, or matched
                // against another loader's copy of the name.
                // A miss is the Java `NoClassDefFoundError` a `Class` static
                // argument's resolution raises, not a VM-internal error.
                let cid = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    thread,
                    current_class_id,
                    &name,
                )
                .map_err(|e| {
                    crate::runtime::exceptions::convert_class_not_found(shared, thread, &name, e)
                })?;
                labels.push(SwitchLabel::Type {
                    class_name: Arc::from(name),
                    class_id: cid,
                });
            }
            RawSwitchLabel::Int(v) => labels.push(SwitchLabel::Int(v)),
            RawSwitchLabel::Long(v) => labels.push(SwitchLabel::Long(v)),
            RawSwitchLabel::Float(v) => labels.push(SwitchLabel::Float(v)),
            RawSwitchLabel::Double(v) => labels.push(SwitchLabel::Double(v)),
            RawSwitchLabel::Str(s) => labels.push(SwitchLabel::Str(Arc::from(s))),
            RawSwitchLabel::PrimitiveClass(desc) => {
                // Interpreter round i4 wave 87, lane L4: the label carries the
                // site's SELECTOR descriptor after its own letter
                // (`primitive_label_spec`), because what a primitive label
                // matches depends on the selector type in JDK 25's generated
                // switch (JEP 507, preview).
                labels.push(SwitchLabel::PrimitiveClass(Arc::from(primitive_label_spec(
                    &desc,
                    &info.target_descriptor,
                ))));
            }
            RawSwitchLabel::EnumDesc {
                class_name,
                constant,
            } => {
                // Through the switching class's loader, as a `Type` label.
                // The JDK resolves an `EnumDesc` label lazily and treats a
                // failure (no such class, not an enum) as "no match", so a
                // miss here is a label that matches nothing, not an error.
                let enum_id = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    thread,
                    current_class_id,
                    &class_name,
                )
                .ok()
                .filter(|&cid| {
                    // `Class.isEnum()`: the direct superclass is `Enum`.
                    let cm = shared.classes.class_manager.read();
                    cm.get_class(cid)
                        .and_then(|c| c.superclass)
                        .and_then(|s| cm.get_class(s))
                        .is_some_and(|s| &*s.name == "java/lang/Enum")
                });
                labels.push(match enum_id {
                    Some(class_id) => SwitchLabel::EnumDesc {
                        class_id,
                        name: Arc::from(constant),
                    },
                    None => SwitchLabel::PrimitiveClass(Arc::from("")),
                });
            }
        }
    }

    // Cache the call site — subsequent executions use pre-resolved ClassIds.
    let site = ResolvedCallSite::TypeSwitch {
        labels: labels.clone(),
    };
    shared.classes.resolution_cache.write().put_call_site_as_of(
        current_class_id,
        cp_index,
        site,
        info.fill_as_of,
    );

    execute_type_switch(shared, thread, frame_idx, &labels)
}

/// Temporary label type used during bootstrap before ClassId resolution.
pub(in crate::runtime::invokedynamic) enum RawSwitchLabel {
    Type(String),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(String),
    PrimitiveClass(String),
    /// A qualified enum constant label: the enum's internal name and the
    /// constant's name (see [`javac_enum_desc_label`]).
    EnumDesc {
        class_name: String,
        constant: String,
    },
}

/// The `(enum class internal name, constant name)` of a `typeSwitch`
/// `EnumDesc` label in the shape javac emits for a qualified enum constant
/// label in a pattern switch (`case E.A`, JEP 441):
///
/// ```text
/// ConstantBootstraps.invoke(EnumDesc.of, <ConstantBootstraps.invoke(ClassDesc.of, "p.E")>, "A")
/// ```
///
/// Decoded from the constant pool rather than by running the two bootstraps:
/// both are side-effect-free JDK factories and the label needs only the two
/// names. `None` for any other shape.
pub(in crate::runtime::invokedynamic) fn javac_enum_desc_label(
    cp: &ConstantPool,
    bootstrap_methods: &[BootstrapMethod],
    cp_index: u16,
) -> Option<(String, String)> {
    let &[class_desc, constant] = constant_bootstraps_invoke_args(
        cp,
        bootstrap_methods,
        cp_index,
        "java/lang/Enum$EnumDesc",
        "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;)Ljava/lang/Enum$EnumDesc;",
    )?
    else {
        return None;
    };
    let &[binary_name] = constant_bootstraps_invoke_args(
        cp,
        bootstrap_methods,
        class_desc,
        "java/lang/constant/ClassDesc",
        "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
    )?
    else {
        return None;
    };
    // `ClassDesc.of` takes a binary name (`p.Outer$E`).
    let class_name = resolve_string_constant(cp, binary_name)?.replace('.', "/");
    Some((class_name, resolve_string_constant(cp, constant)?))
}

/// The arguments after the factory handle of the `CONSTANT_Dynamic` at
/// `cp_index` when it is `ConstantBootstraps.invoke` over the static factory
/// `factory_class.of` with descriptor `factory_desc`; `None` otherwise.
pub(in crate::runtime::invokedynamic) fn constant_bootstraps_invoke_args<'a>(
    cp: &ConstantPool,
    bootstrap_methods: &'a [BootstrapMethod],
    cp_index: u16,
    factory_class: &str,
    factory_desc: &str,
) -> Option<&'a [u16]> {
    let Some(ConstantPoolEntry::Dynamic {
        bootstrap_method_attr_index,
        ..
    }) = cp.get(cp_index)
    else {
        return None;
    };
    let bsm = bootstrap_methods.get(usize::from(*bootstrap_method_attr_index))?;
    let is_static = |h: &MethodHandle, class: &str, name: &str| {
        h.kind == MethodHandleKind::InvokeStatic
            && &*h.class_name == class
            && &*h.member_name == name
    };
    let bsm_handle = resolve_method_handle_full(cp, bsm.bootstrap_method_ref).ok()?;
    if !is_static(&bsm_handle, "java/lang/invoke/ConstantBootstraps", "invoke") {
        return None;
    }
    let (&factory, rest) = bsm.bootstrap_arguments.split_first()?;
    let factory = resolve_method_handle_full(cp, factory).ok()?;
    (is_static(&factory, factory_class, "of") && &*factory.descriptor == factory_desc)
        .then_some(rest)
}

/// The `IndexOutOfBoundsException` of `Objects.checkIndex(restart,
/// labels + 1)`, which the switch the JDK's `SwitchBootstraps` generates makes
/// before anything else (measured on HotSpot 25: `Index -1 out of bounds for
/// length 4`; probe `L4W42SwitchRestartIndex.java`).
/// Interpreter round i1 wave 42, lane L4.
#[cold]
pub(in crate::runtime::invokedynamic) fn switch_restart_out_of_bounds(restart: i32, labels: usize) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::ioobe(format!(
        "Index {restart} out of bounds for length {}",
        labels + 1
    ))))
}

/// Execute a cached `typeSwitch` call site.
///
/// Stack: `[..., target: Object, startIndex: int]` → `[..., matchIndex: int]`
///
/// All Type labels have pre-resolved ClassIds — no lock acquisitions in the loop.
pub fn execute_type_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    labels: &[SwitchLabel],
) -> Result<(), MethodCallFailed> {
    let restart = thread.frames[frame_idx].stack.pop()?;
    let target = thread.frames[frame_idx].stack.pop()?;
    let result = type_switch_index(shared, thread, target, restart, labels)?;
    thread.frames[frame_idx].stack.push(Value::Int(result))?;
    Ok(())
}

/// [`execute_type_switch`] without a frame: the match index for `target` from
/// `restart`, or the error the frame form raises. Shared with the compiled
/// bridge's frame-free switch arm (JIT round 16, lane indy;
/// `CRATONVM_JIT_INDY_SWITCH_FAST`), so the two cannot disagree.
pub(in crate::runtime::invokedynamic) fn type_switch_index(
    shared: &SharedVm,
    thread: &mut JvmThread,
    target: Value,
    restart: Value,
    labels: &[SwitchLabel],
) -> Result<i32, MethodCallFailed> {
    let restart = match restart {
        Value::Int(i) => i,
        _ => 0,
    };
    // The JDK's generated switch checks the restart index first, before the
    // null test (`Objects.checkIndex(restart, labels.length + 1)`; wave 42):
    // it was clamped to 0 and a large one answered the default.
    if restart < 0 || restart as usize > labels.len() {
        return Err(switch_restart_out_of_bounds(restart, labels.len()));
    }
    let start_index = restart as usize;

    let result = match target {
        // Per the `SwitchBootstraps.typeSwitch` contract, a null target returns
        // -1. javac compiles `case null` as the -1 arm of the generated
        // table/lookupswitch (and, when there is no `case null`, emits a null
        // check that throws before the bootstrap is reached), so returning -1
        // here is what the generated bytecode expects.
        Value::Object(None) => -1,
        Value::Object(Some(mut obj_ref)) => {
            let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
            type_switch_match(
                shared,
                thread,
                &mut obj_ref,
                obj_class_id,
                labels,
                start_index,
            )?
        }
        Value::Int(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Int(e) if *e == v)
                || matches!(l, SwitchLabel::PrimitiveClass(d) if primitive_label_matches(d, Some(PrimOperand::Int(v))))
        }),
        Value::Long(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Long(e) if *e == v)
                || matches!(l, SwitchLabel::PrimitiveClass(d) if primitive_label_matches(d, Some(PrimOperand::Long(v))))
        }),
        Value::Float(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Float(e) if e.to_bits() == v.to_bits())
                || matches!(l, SwitchLabel::PrimitiveClass(d) if primitive_label_matches(d, Some(PrimOperand::Float(v))))
        }),
        Value::Double(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Double(e) if e.to_bits() == v.to_bits())
                || matches!(l, SwitchLabel::PrimitiveClass(d) if primitive_label_matches(d, Some(PrimOperand::Double(v))))
        }),
        // Any other (non-target) value matches no label → default arm.
        _ => labels.len() as i32,
    };
    Ok(result)
}

/// `target instanceof <label>` for a `typeSwitch` type-pattern label.
///
/// A bare `is_subclass_of(header class id, label)` — what this used to be — is
/// wrong for two receiver shapes the `instanceof` opcode handles separately:
///
/// * an ARRAY, whose header class id is its COMPONENT class. `case String s`
///   matched a `String[]` target, and `case Object[] a` matched nothing;
/// * a LAMBDA PROXY, whose synthetic class id is absent from the class store,
///   so `case Runnable r` never matched a lambda.
///
/// An array takes the array arm of `op_instanceof` (strict array
/// assignability); every other receiver takes the opcode's own predicate,
/// `interpreter::object_is_instance_of_resolved` — the hierarchy, then the
/// loader-aware, lambda-proxy, synthetic, `$Proxy`, annotation-proxy and
/// display-class fallbacks, in the opcode's order. Before wave 3 only the
/// array and lambda-proxy arms were mirrored here.
///
/// `obj_ref` is refreshed through the `&mut`: the display-class arm can load
/// a class (a safepoint), and pins the receiver across it.
pub(in crate::runtime::invokedynamic) fn type_label_matches(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
    obj_class_id: ClassId,
    label_id: ClassId,
    label_name: &str,
) -> bool {
    if shared.mem.heap.kind_of(*obj_ref) == crate::memory::heap::ObjectKind::Array {
        let Some(desc) = crate::runtime::interpreter::array_descriptor_of(shared, *obj_ref) else {
            return false;
        };
        // `array_is_instance_of` can LOAD a component class (a safepoint),
        // and `type_switch_match` reads `obj_ref` again for the next label:
        // pin it across the call and hand back the current address.
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(*obj_ref);
        let is_instance =
            crate::runtime::interpreter::array_is_instance_of(shared, &desc, label_name);
        *obj_ref = thread.native_pin_roots[pin];
        thread.native_pin_roots.truncate(pin);
        return is_instance;
    }
    if label_name.starts_with('[') {
        // A non-array object is an instance of no array type.
        return false;
    }
    crate::runtime::interpreter::object_is_instance_of_resolved(
        shared,
        thread,
        obj_ref,
        obj_class_id,
        label_id,
        label_name,
    )
}

/// The `name` of an enum constant: `Enum`'s first declared field, slot 0 (the
/// slot `execute_enum_switch` has always read).
pub(in crate::runtime::invokedynamic) fn enum_constant_name(shared: &SharedVm, obj_ref: ObjectRef) -> Option<String> {
    match shared.mem.heap.get_field(obj_ref, 0) {
        Value::Object(Some(name_ref)) => read_java_string(&shared.mem.heap, name_ref),
        _ => None,
    }
}

/// `Enum.getDeclaringClass() == enum_id` for an object of class `obj_class_id`
/// (`enum_id` is a resolved enum class): its class is the enum itself or a
/// constant-body subclass of it. The read lock is only taken for the second.
pub(in crate::runtime::invokedynamic) fn enum_declared_by(shared: &SharedVm, obj_class_id: ClassId, enum_id: ClassId) -> bool {
    obj_class_id == enum_id
        || shared
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .is_some_and(|c| c.superclass == Some(enum_id))
}

/// Match an object reference against switch labels.
///
/// Takes no lock for a label the class hierarchy decides; a type label that
/// the hierarchy refuses consults `instanceof`'s fallbacks, the last of which
/// can load a class. `obj_ref` is refreshed through the `&mut` if it moves.
///
/// The receiver's class NAME is read only when a boxed-constant or
/// primitive-class label is reached (wave 16): a pure type-pattern switch —
/// `case Circle c -> ..; case Square s -> ..` — used to take the VM-wide
/// `class_manager` read lock and bump the name's refcount on every
/// execution for a name none of its labels reads.
pub(in crate::runtime::invokedynamic) fn type_switch_match(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
    obj_class_id: ClassId,
    labels: &[SwitchLabel],
    start_index: usize,
) -> Result<i32, MethodCallFailed> {
    // Read at most once, and only if an `EnumConstant` label is reached.
    let mut enum_name: Option<Option<String>> = None;
    // Read at most once, and only if a label that compares it is reached.
    let mut receiver_name: Option<Arc<str>> = None;
    for (i, label) in labels.iter().enumerate().skip(start_index) {
        let matched = match label {
            SwitchLabel::Type {
                class_id,
                class_name,
            } => {
                // JEP 441: type patterns use instanceof semantics (subclass check).
                // A Long does NOT match `case Integer i` — only exact type or
                // supertype matches are valid.
                type_label_matches(shared, thread, obj_ref, obj_class_id, *class_id, class_name)
            }
            SwitchLabel::EnumConstant(expected) => {
                enum_name
                    .get_or_insert_with(|| enum_constant_name(shared, *obj_ref))
                    .as_deref()
                    == Some(&**expected)
            }
            SwitchLabel::EnumDesc { class_id, name } => {
                // An array's header class id is its COMPONENT class, so an
                // `E[]` target must be refused before the id comparison.
                shared.mem.heap.kind_of(*obj_ref) != crate::memory::heap::ObjectKind::Array
                    && enum_declared_by(shared, obj_class_id, *class_id)
                    && enum_name
                        .get_or_insert_with(|| enum_constant_name(shared, *obj_ref))
                        .as_deref()
                        == Some(&**name)
            }
            SwitchLabel::Int(expected) => {
                // The JDK's matcher (`SwitchBootstraps.generateTypeSwitch`,
                // an `Integer` label): a `Number` target by `intValue()`, a
                // `Character` by `charValue()`, anything else (a `Boolean`
                // included) no match. Wave 41, lane L4: only the `Integer`,
                // `Short`, `Byte`, `Character` and `Boolean` boxes were read,
                // so `5L` and `5.9` missed `case 5` and `Boolean.TRUE`
                // matched `1` (probe `L4W41TypeSwitchConstantLabels`).
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                let value = match switch_int_label_operand(shared, *obj_ref, obj_class_id, name) {
                    SwitchIntOperand::Value(v) => Some(v),
                    SwitchIntOperand::NoMatch => None,
                    SwitchIntOperand::OtherNumber => {
                        switch_number_int_value(shared, thread, obj_ref)?
                    }
                };
                value == Some(*expected)
            }
            SwitchLabel::Long(expected) => {
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                unbox_long(shared, *obj_ref, name) == Some(*expected)
            }
            SwitchLabel::Float(expected) => {
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                unbox_float(shared, *obj_ref, name)
                    .is_some_and(|v| v.to_bits() == expected.to_bits())
            }
            SwitchLabel::Double(expected) => {
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                unbox_double(shared, *obj_ref, name)
                    .is_some_and(|v| v.to_bits() == expected.to_bits())
            }
            SwitchLabel::Str(expected) => {
                // `label.equals(target)`: only a `String` (wave 41, lane L4).
                // The shape reader alone also answered for a `StringBuilder`
                // whose buffer is exactly full (`L4W41TypeSwitchConstantLabels`).
                crate::vm::is_java_lang_string(shared, *obj_ref)
                    && read_java_string(&shared.mem.heap, *obj_ref).as_deref() == Some(&**expected)
            }
            SwitchLabel::PrimitiveClass(spec) => {
                // JEP 507 (preview): a primitive type pattern over a reference
                // selector. Wave 87 (lane L4): JDK 25's generated switch tests
                // `instanceof <the label's own box>` for a non-wrapper selector
                // (`Object o; case int i` matches an Integer only) and the
                // value's exact convertibility for a wrapper selector
                // (`Integer i; case byte b` matches 5, not 1000); this answered
                // any int-family box for any int-family label.
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                let operand = boxed_prim_operand(shared, *obj_ref, name);
                primitive_label_matches_boxed(spec, name, operand)
            }
        };
        if matched {
            return Ok(i as i32);
        }
    }
    // No label matched: the `typeSwitch` contract returns labels.length (the
    // default arm), not -1 (which is reserved for a null target).
    Ok(labels.len() as i32)
}

/// What an `Integer` `typeSwitch` label compares a reference target with.
pub(in crate::runtime::invokedynamic) enum SwitchIntOperand {
    /// The target's `intValue()` / `charValue()`, read without running Java.
    Value(i32),
    /// Neither a `Number` nor a `Character`: no `Integer` label matches.
    NoMatch,
    /// Another `Number` (`AtomicInteger`, `BigInteger`, a user subclass):
    /// its `intValue()` runs ([`switch_number_int_value`]).
    OtherNumber,
}

/// [`SwitchIntOperand`] of a non-null target of class `class_name`. The boxes
/// are read from their value field; `Long` truncates and `Float` / `Double`
/// convert as `l2i` / `f2i` / `d2i` do (Rust's `as` saturates and sends NaN
/// to 0, as Java does). Interpreter round i1 wave 41, lane L4.
pub(in crate::runtime::invokedynamic) fn switch_int_label_operand(
    shared: &SharedVm,
    obj: ObjectRef,
    obj_class_id: ClassId,
    class_name: &str,
) -> SwitchIntOperand {
    let field = || shared.mem.heap.get_field(obj, 0);
    match class_name {
        "java/lang/Integer" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Character" => {
            match field() {
                Value::Int(v) => SwitchIntOperand::Value(v),
                _ => SwitchIntOperand::NoMatch,
            }
        }
        "java/lang/Long" => match field() {
            Value::Long(v) => SwitchIntOperand::Value(v as i32),
            _ => SwitchIntOperand::NoMatch,
        },
        "java/lang/Float" => match field() {
            Value::Float(v) => SwitchIntOperand::Value(v as i32),
            _ => SwitchIntOperand::NoMatch,
        },
        "java/lang/Double" => match field() {
            Value::Double(v) => SwitchIntOperand::Value(v as i32),
            _ => SwitchIntOperand::NoMatch,
        },
        _ => {
            // An array's header class id is its COMPONENT class: an
            // `AtomicInteger[]` is not a `Number`.
            if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
                return SwitchIntOperand::NoMatch;
            }
            let is_number = shared
                .classes
                .class_manager
                .read()
                .is_assignable_to_name(obj_class_id, "java/lang/Number");
            if is_number {
                SwitchIntOperand::OtherNumber
            } else {
                SwitchIntOperand::NoMatch
            }
        }
    }
}

/// `((Number) target).intValue()` for a `Number` that is not a box, run as
/// the JDK's matcher runs it (its exception is the switch's). `None` when the
/// call answers no `int`. The target is pinned across the call and handed
/// back through `obj_ref`.
pub(in crate::runtime::invokedynamic) fn switch_number_int_value(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
) -> Result<Option<i32>, MethodCallFailed> {
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(*obj_ref);
    let result = {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.invoke_virtual(*obj_ref, "intValue", "()I", &[])
    };
    *obj_ref = thread.native_pin_roots[pin];
    thread.native_pin_roots.truncate(pin);
    Ok(match result? {
        Some(Value::Int(v)) => Some(v),
        _ => None,
    })
}

/// The class name of a `typeSwitch` receiver of class `class_id`, read into
/// `cell` on first use (one `class_manager` read) and borrowed from it after.
/// The empty name for a class the store does not know (a lambda proxy), which
/// matches no boxed-type label — the answer the eager read gave.
pub(in crate::runtime::invokedynamic) fn switch_receiver_name<'a>(
    shared: &SharedVm,
    class_id: ClassId,
    cell: &'a mut Option<Arc<str>>,
) -> &'a str {
    cell.get_or_insert_with(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    })
}

/// JEP 507: Check if a boxed primitive value matches a target wrapper type
/// via widening or narrowing conversion.
///
/// For example, a boxed `Integer(42)` matches `java/lang/Long` (widening)
/// and a boxed `Integer(5)` matches `java/lang/Byte` (narrowing, value in range).
pub(in crate::runtime::invokedynamic) fn primitive_pattern_match(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    obj_class_name: &str,
    target_class_name: &str,
) -> bool {
    // Extract the numeric value from the source wrapper.
    let source_value = match obj_class_name {
        "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Integer"
        | "java/lang/Character"
        | "java/lang/Boolean" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Int(v) => NumericValue::Int(v),
            _ => return false,
        },
        "java/lang/Long" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Long(v) => NumericValue::Long(v),
            _ => return false,
        },
        "java/lang/Float" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Float(v) => NumericValue::Float(v),
            _ => return false,
        },
        "java/lang/Double" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Double(v) => NumericValue::Double(v),
            _ => return false,
        },
        _ => return false,    };

    // Try to convert to target type.
    match target_class_name {
        "java/lang/Byte" => match source_value {
            NumericValue::Int(v) => v >= i8::MIN as i32 && v <= i8::MAX as i32,
            NumericValue::Long(v) => v >= i8::MIN as i64 && v <= i8::MAX as i64,
            _ => false,
        },
        "java/lang/Short" => match source_value {
            NumericValue::Int(v) => v >= i16::MIN as i32 && v <= i16::MAX as i32,
            NumericValue::Long(v) => v >= i16::MIN as i64 && v <= i16::MAX as i64,
            _ => false,
        },
        "java/lang/Character" => match source_value {
            NumericValue::Int(v) => v >= 0 && v <= u16::MAX as i32,
            NumericValue::Long(v) => v >= 0 && v <= u16::MAX as i64,
            _ => false,
        },
        "java/lang/Integer" => match source_value {
            NumericValue::Int(_) => true, // same type always matches
            NumericValue::Long(v) => v >= i32::MIN as i64 && v <= i32::MAX as i64,
            NumericValue::Float(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i32::MIN as f32
                    && v <= i32::MAX as f32
                    && v == (v as i32) as f32
            }
            NumericValue::Double(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i32::MIN as f64
                    && v <= i32::MAX as f64
                    && v == (v as i32) as f64
            }
        },
        "java/lang/Long" => match source_value {
            NumericValue::Int(v) => {
                // Widening: int -> long always succeeds.
                let _ = v;
                true
            }
            NumericValue::Long(_) => true,
            NumericValue::Float(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i64::MIN as f32
                    && v <= i64::MAX as f32
                    && v == (v as i64) as f32
            }
            NumericValue::Double(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i64::MIN as f64
                    && v <= i64::MAX as f64
                    && v == (v as i64) as f64
            }
        },
        "java/lang/Float" => match source_value {
            NumericValue::Int(v) => {
                // Widening: int -> float (may lose precision, but allowed as widening).
                let _ = v;
                true
            }
            NumericValue::Long(v) => {
                let _ = v;
                true
            }
            NumericValue::Float(_) => true,
            NumericValue::Double(v) => {
                // Narrowing: double -> float only if exact.
                !v.is_nan() && v == (v as f32) as f64
            }
        },
        "java/lang/Double" => match source_value {
            // Widening to double always succeeds from any numeric type.
            NumericValue::Int(_) | NumericValue::Long(_) | NumericValue::Float(_) => true,
            NumericValue::Double(_) => true,
        },
        _ => false,
    }
}

/// A numeric value extracted from a boxed primitive wrapper.
pub(in crate::runtime::invokedynamic) enum NumericValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

/// Search labels for a primitive match.
pub(in crate::runtime::invokedynamic) fn primitive_match(
    labels: &[SwitchLabel],
    start: usize,
    pred: impl Fn(&SwitchLabel) -> bool,
) -> i32 {
    for (i, label) in labels.iter().enumerate().skip(start) {
        if pred(label) {
            return i as i32;
        }
    }
    // No label matched → default arm index (labels.length), not -1.
    labels.len() as i32
}

// Unbox a numeric wrapper to i32 (Integer, Byte, Short, Character, Boolean).
// ---------------------------------------------------------------------------
// Primitive type labels of `typeSwitch` (JEP 507, preview in JDK 25;
// interpreter round i4 wave 87, lane L4; page
// `interpreter-i86-L4-lambda-and-switch-residuals-FIXED-20261120`, residual 2). JDK 25's
// `SwitchBootstraps.generateTypeSwitch` decides a primitive `Class` label by
// the SELECTOR type: `isNotValidPair` (boolean against a numeric primitive),
// `unconditionalExactnessCheck` (the same type, or a widening that is exact),
// `instanceof <the label's own box>` for a reference selector that is not a
// wrapper, and otherwise the value's `ExactConversionsSupport.is<From>To<To>Exact`
// test. A label spec is the label's descriptor letter followed by the
// selector's descriptor (`primitive_label_spec`); an empty spec matches
// nothing.
// ---------------------------------------------------------------------------

/// A primitive selector value, or a wrapper selector's unboxed value.
#[derive(Clone, Copy, Debug)]
pub(in crate::runtime::invokedynamic) enum PrimOperand {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

/// `label` (a one-letter primitive descriptor) followed by the first
/// parameter descriptor of `target_descriptor` (the selector); empty when
/// either cannot be read.
pub(in crate::runtime::invokedynamic) fn primitive_label_spec(label: &str, target_descriptor: &str) -> String {
    if label.len() != 1 {
        return String::new();
    }
    let Some(params) = target_descriptor.strip_prefix('(') else {
        return String::new();
    };
    let bytes = params.as_bytes();
    let mut end = 0;
    while bytes.get(end) == Some(&b'[') {
        end += 1;
    }
    match bytes.get(end) {
        Some(b'L') => match params[end..].find(';') {
            Some(semi) => end += semi + 1,
            None => return String::new(),
        },
        Some(b')') | None => return String::new(),
        Some(_) => end += 1,
    }
    format!("{label}{}", &params[..end])
}

/// The primitive letter of a wrapper class's descriptor, or `None`.
pub(in crate::runtime::invokedynamic) fn wrapper_letter_of_descriptor(descriptor: &str) -> Option<u8> {
    wrapper_letter_of_name(descriptor.strip_prefix('L')?.strip_suffix(';')?)
}

/// The primitive letter of a wrapper class's internal name, or `None`.
pub(in crate::runtime::invokedynamic) fn wrapper_letter_of_name(name: &str) -> Option<u8> {
    Some(match name {
        "java/lang/Integer" => b'I',
        "java/lang/Byte" => b'B',
        "java/lang/Short" => b'S',
        "java/lang/Character" => b'C',
        "java/lang/Boolean" => b'Z',
        "java/lang/Long" => b'J',
        "java/lang/Float" => b'F',
        "java/lang/Double" => b'D',
        _ => return None,
    })
}

/// `unconditionalExactnessCheck` for two primitive letters: the same type, or
/// a widening that is always exact (not `byte->char`, `int->float`,
/// `long->float`, `long->double`).
pub(in crate::runtime::invokedynamic) fn primitive_unconditionally_exact(from: u8, to: u8) -> bool {
    from == to
        || matches!(
            (from, to),
            (b'B', b'S' | b'I' | b'J' | b'F' | b'D')
                | (b'S', b'I' | b'J' | b'F' | b'D')
                | (b'C', b'I' | b'J' | b'F' | b'D')
                | (b'I', b'J' | b'D')
                | (b'F', b'D')
        )
}

/// `ExactConversionsSupport.is<From>To<To>Exact` for `value` read as the
/// selector's primitive `from` (byte/short/char/int all read as `int`).
pub(in crate::runtime::invokedynamic) fn primitive_exact_conversion(from: u8, to: u8, value: PrimOperand) -> bool {
    let neg_zero_f = |n: f32| n.to_bits() == 0x8000_0000;
    let neg_zero_d = |n: f64| n.to_bits() == 0x8000_0000_0000_0000;
    match from {
        b'B' | b'S' | b'C' | b'I' => {
            let n = match value {
                PrimOperand::Int(v) => v,
                PrimOperand::Long(v) => v as i32,
                PrimOperand::Float(v) => v as i32,
                PrimOperand::Double(v) => v as i32,
            };
            match to {
                b'B' => n == i32::from(n as i8),
                b'S' => n == i32::from(n as i16),
                b'C' => n == i32::from(n as u16),
                b'F' => (n as f32) as i32 == n && n != i32::MAX,
                b'I' | b'J' | b'D' => true,
                _ => false,
            }
        }
        b'J' => {
            let n = match value {
                PrimOperand::Long(v) => v,
                PrimOperand::Int(v) => i64::from(v),
                PrimOperand::Float(v) => v as i64,
                PrimOperand::Double(v) => v as i64,
            };
            match to {
                b'B' => n == i64::from(n as i8),
                b'S' => n == i64::from(n as i16),
                b'C' => n == i64::from(n as u16),
                b'I' => n == i64::from(n as i32),
                b'F' => (n as f32) as i64 == n && n != i64::MAX,
                b'D' => (n as f64) as i64 == n && n != i64::MAX,
                b'J' => true,
                _ => false,
            }
        }
        b'F' => {
            let n = match value {
                PrimOperand::Float(v) => v,
                PrimOperand::Int(v) => v as f32,
                PrimOperand::Long(v) => v as f32,
                PrimOperand::Double(v) => v as f32,
            };
            match to {
                b'B' => n == f32::from(n as i32 as i8) && !neg_zero_f(n),
                b'S' => n == f32::from(n as i32 as i16) && !neg_zero_f(n),
                b'C' => n == f32::from(n as i32 as u16) && !neg_zero_f(n),
                b'I' => n == (n as i32) as f32 && n != 2_147_483_648.0 && !neg_zero_f(n),
                b'J' => {
                    n == (n as i64) as f32 && n != 9_223_372_036_854_775_808.0 && !neg_zero_f(n)
                }
                b'F' | b'D' => true,
                _ => false,
            }
        }
        b'D' => {
            let n = match value {
                PrimOperand::Double(v) => v,
                PrimOperand::Int(v) => f64::from(v),
                PrimOperand::Long(v) => v as f64,
                PrimOperand::Float(v) => f64::from(v),
            };
            match to {
                b'B' => n == f64::from(n as i32 as i8) && !neg_zero_d(n),
                b'S' => n == f64::from(n as i32 as i16) && !neg_zero_d(n),
                b'C' => n == f64::from(n as i32 as u16) && !neg_zero_d(n),
                b'I' => n == f64::from(n as i32) && !neg_zero_d(n),
                b'J' => {
                    n == (n as i64) as f64 && n != 9_223_372_036_854_775_808.0 && !neg_zero_d(n)
                }
                b'F' => n == f64::from(n as f32) || n.is_nan(),
                b'D' => true,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Does the primitive label `spec` match `operand`, for a PRIMITIVE or
/// WRAPPER selector (`None`: the wrapper's value is no `Number` /
/// `Character` the generated switch reads, which matches only an
/// unconditional label)?
pub(in crate::runtime::invokedynamic) fn primitive_label_matches(spec: &str, operand: Option<PrimOperand>) -> bool {
    let bytes = spec.as_bytes();
    let Some(&label) = bytes.first() else {
        return false;
    };
    let selector = &spec[1..];
    let selector_primitive = (selector.len() == 1).then(|| selector.as_bytes()[0]);
    if let Some(s) = selector_primitive {
        // `isNotValidPair`.
        if (s == b'Z') != (label == b'Z') {
            return false;
        }
        if primitive_unconditionally_exact(s, label) {
            return true;
        }
    }
    let Some(from) = selector_primitive.or_else(|| wrapper_letter_of_descriptor(selector)) else {
        return false;
    };
    if primitive_unconditionally_exact(from, label) {
        return true;
    }
    if label == b'Z' || from == b'Z' {
        return false;
    }
    operand.is_some_and(|v| primitive_exact_conversion(from, label, v))
}

/// The value a box of class `box_name` holds, as the generated switch reads it
/// for the selector's letter: a `Number`'s `xValue()`, a `Character`'s
/// `charValue()` only for an int-family selector; `None` otherwise.
pub(in crate::runtime::invokedynamic) fn boxed_prim_operand(shared: &SharedVm, obj: ObjectRef, box_name: &str) -> Option<PrimOperand> {
    let letter = wrapper_letter_of_name(box_name)?;
    match (letter, shared.mem.heap.get_field(obj, 0)) {
        (b'Z', _) => None,
        (_, Value::Int(v)) => Some(PrimOperand::Int(v)),
        (_, Value::Long(v)) => Some(PrimOperand::Long(v)),
        (_, Value::Float(v)) => Some(PrimOperand::Float(v)),
        (_, Value::Double(v)) => Some(PrimOperand::Double(v)),
        _ => None,
    }
}

/// A primitive label against a non-null REFERENCE target of class
/// `box_name`: `instanceof <the label's box>` for a selector that is not a
/// wrapper, else [`primitive_label_matches`] on the unboxed value (a
/// `Character` value is read only for an int-family selector).
pub(in crate::runtime::invokedynamic) fn primitive_label_matches_boxed(spec: &str, box_name: &str, operand: Option<PrimOperand>) -> bool {
    let Some(&label) = spec.as_bytes().first() else {
        return false;
    };
    let selector = &spec[1..];
    if selector.len() == 1 {
        return primitive_label_matches(spec, operand);
    }
    let Some(from) = wrapper_letter_of_descriptor(selector) else {
        return wrapper_letter_of_name(box_name) == Some(label);
    };
    let operand = match wrapper_letter_of_name(box_name) {
        Some(b'C') if matches!(from, b'J' | b'F' | b'D') => None,
        _ => operand,
    };
    primitive_label_matches(spec, operand)
}

pub(in crate::runtime::invokedynamic) fn unbox_long(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<i64> {
    if class_name == "java/lang/Long" {
        match shared.mem.heap.get_field(obj, 0) {
            Value::Long(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

pub(in crate::runtime::invokedynamic) fn unbox_float(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<f32> {
    if class_name == "java/lang/Float" {
        match shared.mem.heap.get_field(obj, 0) {
            Value::Float(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

pub(in crate::runtime::invokedynamic) fn unbox_double(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<f64> {
    if class_name == "java/lang/Double" {
        match shared.mem.heap.get_field(obj, 0) {
            Value::Double(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

// ===========================================================================
// ObjectMethods.bootstrap — record equals/hashCode/toString (JEP 395)
// ===========================================================================

/// Bootstrap `ObjectMethods.bootstrap` for record classes.
///
/// Bootstrap arguments:
///   arg[0]: Class — the record class
///   arg[1]: String — component names separated by `;`
///   arg[2..]: MethodHandle — getField handles for each component
///
/// The target name (`info.target_name`) tells us which method:
///   "equals", "hashCode", or "toString".
///
/// Interpreter round i1 wave 39, lane L4
/// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`): javac's shape
/// (`Class::object_methods_args_are_canonical`) links exactly as before.
/// Any other getter list whose every getter is a `REF_getField` of the record
/// class naming an instance field links GETTER-DRIVEN in every mode: one slot
/// and descriptor per getter, which `equals`/`hashCode`/`toString` fold over
/// as the JDK's `ObjectMethods` does (they used to walk the record's own
/// components, and `toString` read positional slots). Any other getter (an
/// accessor method, another class's field) links through the JDK's own
/// `ObjectMethods` under `--real-jdk` (`bootstrap_generic`); `--compatible`
/// keeps the positional reading. Wave 41: under `--real-jdk` a `toString`
/// site whose getters are the record's fields and accessor METHODS links
/// getter-driven too, calling each accessor (the JDK's `makeToString` needs
/// `MethodHandle.copyWith`, which CratonVM lacks). Wave 42: static getters
/// `X.m(R)T` and special getters `R.m()T` (from `R`) too; a getter whose
/// handle type is not `(R)T` was refused before this runs
/// (`object_methods_to_string_getter_refusal`).
/// `--real-jdk`: an `ObjectMethods` `toString` site whose getters include a
/// METHOD handle (`REF_invokeVirtual` / `REF_invokeStatic` /
/// `REF_invokeSpecial`, never javac's shape) is linked by the JDK's own
/// `ObjectMethods.bootstrap` (`bootstrap_generic`) when `true`, and by the
/// native accessor-getter models of waves 41-42 (`RecordAccessorGetter`) when
/// `false`. Those models existed because the JDK route failed in
/// `MethodHandle.copyWith` before wave 43. `--compatible` is unaffected (it
/// never modelled them). Interpreter round i1 wave 44, lane L4
/// (`i43-L4-proposal-retire-the-copywith-workarounds`, step 3); the positive
/// control is `CRATONVM_DBG_INDY_ALL=1`'s `accessor-method getters take the
/// jdk route` line on `L4W42ObjectMethodsStaticSpecialGetters`.
pub(in crate::runtime::invokedynamic) const OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE: bool = true;

pub(in crate::runtime::invokedynamic) fn bootstrap_record_object_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    let method = match info.target_name.as_str() {
        "equals" => RecordMethodKind::Equals,
        "hashCode" => RecordMethodKind::HashCode,
        "toString" => RecordMethodKind::ToString,
        other => {
            return Err(VmError::Internal {
                message: format!("ObjectMethods.bootstrap: unknown target '{other}'"),
            }
            .into());
        }
    };

    // Parse component names and field descriptors from bootstrap arguments.
    let (component_names, record_class_name, canonical, field_getters, field_descriptors) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("ObjectMethods bootstrap: class {current_class_id} not found"),
            })?;

        // arg[0] = Class reference (the record class) — get its record components
        // arg[1] = String with component names separated by `;`
        let names_str = if info.bootstrap_arg_indices.len() >= 2 {
            resolve_string_constant(&class.constant_pool, info.bootstrap_arg_indices[1])
                .unwrap_or_default()
        } else {
            String::new()
        };
        let component_names: Vec<Arc<str>> = names_str
            .split(';')
            .filter(|s| !s.is_empty())
            .map(Arc::from)
            .collect();

        // arg[2..] = MethodHandle getters — extract field descriptors from them.
        // Each MethodHandle is a REF_getField for the record component.
        let mut field_descriptors: Vec<Arc<str>> = Vec::with_capacity(component_names.len());
        for &arg_idx in info.bootstrap_arg_indices.iter().skip(2) {
            let desc = resolve_method_handle_field_descriptor(&class.constant_pool, arg_idx)
                .unwrap_or_else(|| "I".to_string());
            field_descriptors.push(Arc::from(desc));
        }
        // The same getters, one entry each: the resolved handle (of any
        // kind; `None` for an entry that is not a method handle).
        let field_getters: Vec<Option<MethodHandle>> = info
            .bootstrap_arg_indices
            .iter()
            .skip(2)
            .map(|&arg_idx| resolve_method_handle_full(&class.constant_pool, arg_idx).ok())
            .collect();
        let canonical = class.object_methods_args_are_canonical(&info.bootstrap_arg_indices);

        // arg[0]'s name; the record class is resolved below, after the read
        // lock is dropped (class loading takes the write lock).
        let record_class_name = if !info.bootstrap_arg_indices.is_empty() {
            class
                .constant_pool
                .get_class_name(info.bootstrap_arg_indices[0])
                .map(|s| s.to_string())
        } else {
            None
        };

        (
            component_names,
            record_class_name,
            canonical,
            field_getters,
            field_descriptors,
        )
    };

    // Loader-aware for the same reason as the typeSwitch labels: the record
    // class is a `CONSTANT_Class` of the calling class (almost always the
    // record itself), which a built-in-chain-only lookup cannot see when a
    // user loader defined it. A miss is the resolution's
    // `NoClassDefFoundError`, as for the switch bootstraps' labels (it was the
    // raw internal error).
    let rec_cid: Option<ClassId> = match record_class_name {
        Some(ref rec_name) => Some(
            crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                rec_name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found(shared, thread, rec_name, e)
            })?,
        ),
        None => None,
    };

    // Getter-driven slots: `Some` only when the site is not javac's shape and
    // every getter is a `REF_getField` of the record class naming an instance
    // field, found as JVMS 5.4.3.2 resolves the reference from that class.
    //
    // Wave 41, lane L4 (`i38-L4-objectmethods-native-linkage-ignores-its-getters`):
    // a `toString` site under `--real-jdk` also models an accessor-METHOD
    // getter of the record class (`REF_invokeVirtual R.b()T`, `T` not
    // `void`), invoked on the receiver, as the JDK's `makeToString` filters
    // the receiver through it. The JDK's own route for such a site needs
    // `MethodHandle.copyWith` (LambdaForms), which CratonVM lacks. `equals` /
    // `hashCode` of such a site keep the JDK route, which links. A getter of
    // another class keeps the JDK route too (its handle's type is not
    // `(R)T`, which `makeToString`'s `permuteArguments` refuses).
    //
    // Wave 42, lane L4 (same page): a `toString` site under `--real-jdk` also
    // models `REF_invokeStatic X.m(R)T` of any class `X` (the receiver is its
    // argument) and `REF_invokeSpecial R.m()T` from `R` itself (`R`'s method,
    // never an override): both have the handle type `(R)T` that
    // `makeToString` accepts. The owner of a static getter of another class
    // is resolved here, through the calling class's loader (the getters were
    // already resolved as `ldc` of them by `object_methods_resolve_getters`,
    // so this finds a loaded class).
    //
    // Wave 44, lane L4 (`i43-L4-proposal-retire-the-copywith-workarounds`,
    // step 3): with `MethodHandle.copyWith` registered (wave 43) the JDK's
    // own `ObjectMethods.makeToString` links these method getters; the host
    // measured it on direct `ObjectMethods.bootstrap` calls
    // (`L4W43ObjectMethodsJdkRoute`, `special-*` / `static-*` / `ctor-*`,
    // first wave-43 chain). [`OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE`]
    // hands such a site to `bootstrap_generic` instead of the models above.
    let model_accessor_methods = method == RecordMethodKind::ToString
        && shared.classes.indy_link_checks
        && !OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE;
    if dbg_indy_all()
        && OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE
        && method == RecordMethodKind::ToString
        && shared.classes.indy_link_checks
        && !canonical
        && field_getters.iter().flatten().any(|h| {
            matches!(
                h.kind,
                MethodHandleKind::InvokeStatic
                    | MethodHandleKind::InvokeVirtual
                    | MethodHandleKind::InvokeSpecial
            )
        })
    {
        eprintln!(
            "[indy-all] object-methods toString cp#{cp_index}: accessor-method getters take the jdk route"
        );
    }
    let static_owners: Vec<Option<ClassId>> = if model_accessor_methods && !canonical {
        let mut owners = Vec::with_capacity(field_getters.len());
        for getter in &field_getters {
            owners.push(match getter {
                Some(h) if h.kind == MethodHandleKind::InvokeStatic => {
                    crate::runtime::interpreter::resolve_class_loader_aware(
                        shared,
                        thread,
                        current_class_id,
                        &h.class_name,
                    )
                    .ok()
                }
                _ => None,
            });
        }
        owners
    } else {
        Vec::new()
    };
    let mut accessor_getters: Vec<Option<RecordAccessorGetter>> = Vec::new();
    let getter_slots: Option<(ClassId, Vec<usize>, Vec<Arc<str>>)> =
        match (rec_cid, record_class_name.as_deref()) {
            (Some(rec_cid), Some(rec_name)) if !canonical => {
                let cm = shared.classes.class_manager.read();
                let mut slots: Vec<usize> = Vec::with_capacity(field_getters.len());
                let mut descriptors: Vec<Arc<str>> = Vec::with_capacity(field_getters.len());
                let mut methods: Vec<Option<RecordAccessorGetter>> =
                    Vec::with_capacity(field_getters.len());
                let mut all_modelled = true;
                let record_desc = format!("L{rec_name};");
                let caller_is_record = current_class_id == rec_cid;
                for (gi, getter) in field_getters.iter().enumerate() {
                    let Some(h) = getter else {
                        all_modelled = false;
                        break;
                    };
                    if h.kind == MethodHandleKind::InvokeStatic && model_accessor_methods {
                        // `(R)T`, `T` not `void`, of any class.
                        let (params, ret) =
                            crate::runtime::interpreter::split_method_descriptor_ref(&h.descriptor);
                        let owner = static_owners.get(gi).copied().flatten();
                        match (params.as_slice(), owner) {
                            ([p], Some(owner))
                                if *p == record_desc && !ret.is_empty() && ret != "V" =>
                            {
                                slots.push(usize::MAX);
                                descriptors.push(Arc::from(ret));
                                methods.push(Some(RecordAccessorGetter::Static {
                                    owner,
                                    owner_name: Arc::clone(&h.class_name),
                                    name: Arc::clone(&h.member_name),
                                    descriptor: Arc::clone(&h.descriptor),
                                }));
                                continue;
                            }
                            _ => {
                                all_modelled = false;
                                break;
                            }
                        }
                    }
                    if &*h.class_name != rec_name {
                        all_modelled = false;
                        break;
                    }
                    let method_kind = match h.kind {
                        MethodHandleKind::InvokeVirtual => Some(false),
                        // From `R` itself only: from another class the
                        // handle's receiver type is that class, not `R`.
                        MethodHandleKind::InvokeSpecial if caller_is_record => Some(true),
                        _ => None,
                    };
                    if let (Some(special), true) = (method_kind, model_accessor_methods) {
                        // `()T`, `T` not `void`: the return type is what
                        // `toString` renders.
                        match h.descriptor.strip_prefix("()") {
                            Some(ret) if !ret.is_empty() && ret != "V" => {
                                slots.push(usize::MAX);
                                descriptors.push(Arc::from(ret));
                                methods.push(Some(if special {
                                    RecordAccessorGetter::Special {
                                        owner: rec_cid,
                                        owner_name: Arc::clone(&h.class_name),
                                        name: Arc::clone(&h.member_name),
                                        descriptor: Arc::clone(&h.descriptor),
                                    }
                                } else {
                                    RecordAccessorGetter::Virtual {
                                        name: Arc::clone(&h.member_name),
                                        descriptor: Arc::clone(&h.descriptor),
                                    }
                                }));
                                continue;
                            }
                            _ => {
                                all_modelled = false;
                                break;
                            }
                        }
                    }
                    if h.kind != MethodHandleKind::GetField {
                        all_modelled = false;
                        break;
                    }
                    match crate::classloading::find_field_recursive_by_descriptor(
                        rec_cid,
                        &h.member_name,
                        &h.descriptor,
                        &cm.class_store,
                    ) {
                        Some((slot, field, _)) if !field.is_static() => {
                            slots.push(slot);
                            descriptors.push(Arc::clone(&h.descriptor));
                            methods.push(None);
                        }
                        _ => {
                            all_modelled = false;
                            break;
                        }
                    }
                }
                if all_modelled && methods.iter().any(Option::is_some) {
                    accessor_getters = methods;                }
                all_modelled.then_some((rec_cid, slots, descriptors))            }
            _ => None,
        };

    if !canonical && getter_slots.is_none() && shared.classes.indy_link_checks {
        // A getter the native linkage does not model: the JDK's own
        // `ObjectMethods` builds the call site (or raises its error).
        if dbg_indy_all() {
            eprintln!(
                "[indy-all] object-methods {} cp#{cp_index}: jdk bootstrap (getters not modelled)",
                info.target_name
            );
        }
        return bootstrap_generic(shared, thread, frame_idx, cp_index, info, current_class_id);
    }

    let (field_indices, field_descriptors, getter_driven) = match getter_slots {
        Some((rec_cid, slots, descriptors)) => {
            if dbg_indy_all() {
                eprintln!(
                    "[indy-all] object-methods {} cp#{cp_index}: getter-driven ({} getters, {} accessor method(s))",
                    info.target_name,
                    slots.len(),
                    accessor_getters.iter().filter(|g| g.is_some()).count()
                );
            }
            (slots, descriptors, Some(rec_cid))
        }
        None => {
            // javac's shape, or (`--compatible`) a getter list the native
            // linkage does not model: component `i` is slot
            // `first_field_index + i` of the record class.
            let first_field = rec_cid.and_then(|id| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(id)
                    .map(|c| c.first_field_index)
            });
            let field_indices: Vec<usize> = match first_field {
                Some(first_field) => (0..component_names.len())
                    .map(|i| first_field + i)
                    .collect(),
                None => (0..component_names.len()).collect(),
            };
            (field_indices, field_descriptors, None)
        }
    };

    // Cache the call site.
    let site = ResolvedCallSite::RecordObjectMethod {
        method,
        component_names: component_names.clone(),
        field_indices: field_indices.clone(),
        field_descriptors: field_descriptors.clone(),
        getter_driven,
        accessor_getters: accessor_getters.clone(),
    };
    shared.classes.resolution_cache.write().put_call_site_as_of(
        current_class_id,
        cp_index,
        site,
        info.fill_as_of,
    );

    execute_record_object_method(
        shared,
        thread,
        frame_idx,
        method,
        &component_names,
        &field_indices,
        &field_descriptors,
        getter_driven,
        &accessor_getters,
    )
}

// Resolve a MethodHandle CP entry to extract the field descriptor.
/// Used for ObjectMethods bootstrap to determine component types.
pub(in crate::runtime::invokedynamic) fn resolve_method_handle_field_descriptor(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index) {
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind: _,
            reference_index,
        }) => {
            // The reference_index points to a FieldReference
            match cp.get(*reference_index) {
                Some(ConstantPoolEntry::FieldReference {
                    name_and_type_index,
                    ..
                }) => {
                    let (_, desc) = cp.get_name_and_type(*name_and_type_index)?;
                    Some(desc.to_string())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Execute a record ObjectMethods call site.
///
/// For `toString`: Stack `[..., this]` → `[..., String]`
/// For `hashCode`: Stack `[..., this]` → `[..., int]`
/// For `equals`:   Stack `[..., this, other]` → `[..., boolean]`
///
/// gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
/// the components' `toString` / `hashCode` / `equals` run Java, so an
/// allocation here cannot be retried; it takes the unwinding arm instead of
/// the infallible one. The operand stack may be left popped when the unwind
/// lands; the `Err` makes the frame throw, as the arms' own `?` returns do.
pub(in crate::runtime::invokedynamic) fn execute_record_object_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method: RecordMethodKind,
    component_names: &[Arc<str>],
    field_indices: &[usize],
    field_descriptors: &[Arc<str>],
    getter_driven: Option<ClassId>,
    accessor_getters: &[Option<RecordAccessorGetter>],
) -> Result<(), MethodCallFailed> {
    // The operands come off the stack first (`equals` pops `other`, then
    // `this`), the answer goes back on afterwards; everything in between is
    // [`record_object_method_value`], which the compiled bridge's frame-free
    // record arm calls with the same operands as values. Neither a pop nor a
    // push allocates, so the order against `catch_alloc_oom` is immaterial.
    let site_class_id = thread.frames[frame_idx].class_id;
    let mut args: [Value; 2] = [Value::Object(None); 2];
    let arity = record_object_method_arity(method);
    for slot in args[..arity].iter_mut().rev() {
        *slot = thread.frames[frame_idx].stack.pop()?;
    }
    let value = record_object_method_value(
        shared,
        thread,
        site_class_id,
        method,
        &args[..arity],
        component_names,
        field_indices,
        field_descriptors,
        getter_driven,
        accessor_getters,
    )?;
    thread.frames[frame_idx].stack.push(value)?;
    Ok(())
}

/// How many operands a record `ObjectMethods` method takes: `equals` the
/// receiver and the other object, `hashCode` / `toString` the receiver.
#[inline]
pub(in crate::runtime::invokedynamic) fn record_object_method_arity(method: RecordMethodKind) -> usize {
    match method {
        RecordMethodKind::Equals => 2,
        RecordMethodKind::HashCode | RecordMethodKind::ToString => 1,
    }
}

/// The value a linked record `ObjectMethods` site answers for `args` (the
/// receiver, and `other` for `equals`; [`record_object_method_arity`] of them,
/// in source order). `site_class_id` is the class whose instruction it is --
/// the record class for javac's shape -- which a null receiver's
/// `equals` / `toString` names when the site is not getter-driven.
///
/// The interpreter's executor ([`execute_record_object_method`]) and the
/// compiled bridge's frame-free record arm (`CRATONVM_JIT_INDY_RECORD_FAST`)
/// both answer through here, so the two cannot disagree. The `catch_alloc_oom`
/// unwinding arm is gc-common w6-c's (see the executor's doc).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::invokedynamic) fn record_object_method_value(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site_class_id: ClassId,
    method: RecordMethodKind,
    args: &[Value],
    component_names: &[Arc<str>],
    field_indices: &[usize],
    field_descriptors: &[Arc<str>],
    getter_driven: Option<ClassId>,
    accessor_getters: &[Option<RecordAccessorGetter>],
) -> Result<Value, MethodCallFailed> {
    match crate::runtime::native_oom::catch_alloc_oom(|| {
        record_object_method_value_inner(
            shared,
            thread,
            site_class_id,
            method,
            args,
            component_names,
            field_indices,
            field_descriptors,
            getter_driven,
            accessor_getters,
        )
    }) {
        Ok(r) => r,
        Err(oom) => Err(MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::invokedynamic) fn record_object_method_value_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site_class_id: ClassId,
    method: RecordMethodKind,
    args: &[Value],
    component_names: &[Arc<str>],
    field_indices: &[usize],
    field_descriptors: &[Arc<str>],
    getter_driven: Option<ClassId>,
    accessor_getters: &[Option<RecordAccessorGetter>],
) -> Result<Value, MethodCallFailed> {
    use cratonvm_native_builtins::intrinsics::record as rec;
    let operand = |i: usize| args.get(i).copied().unwrap_or(Value::Object(None));
    match method {
        RecordMethodKind::Equals => {
            let (this, other) = (operand(0), operand(1));
            let result = match (this, other) {
                (Value::Object(Some(a)), Value::Object(Some(b))) => {
                    let mut ctx = NativeContextImpl { shared, thread };
                    // A getter-driven site (wave 39) folds over its own
                    // getters; javac's shape over the record's components.
                    i32::from(match getter_driven {
                        Some(record_class) => rec::record_equals_by_getters(
                            &mut ctx,
                            a,
                            b,
                            record_class,
                            field_indices,
                        )?,
                        None => rec::record_equals(&mut ctx, a, b)?,
                    })
                }
                // A NULL receiver (a hand-assembled site; wave 39), as
                // `ObjectMethods.makeEquals` answers it: the same object as a
                // null operand; against an instance of the record class its
                // getters dereference it (`NullPointerException`), unless
                // there are none; any other operand is not an instance.
                (Value::Object(None), Value::Object(None)) => 1,
                (Value::Object(None), Value::Object(Some(b))) => {
                    let record_class = getter_driven.unwrap_or(site_class_id);
                    let instance = shared.mem.heap.kind_of(b) == cratonvm_types::ObjectKind::Object
                        && shared
                            .classes
                            .class_manager
                            .read()
                            .is_subclass_of(shared.mem.heap.class_id_of(b), record_class);
                    match (instance, field_indices.is_empty()) {
                        (false, _) => 0,
                        (true, true) => 1,
                        (true, false) => return Err(record_null_receiver()),
                    }
                }
                // `x.equals(null)`, and a non-reference operand, are false.
                _ => 0,
            };
            Ok(Value::Int(result))
        }
        RecordMethodKind::HashCode => {
            let hash = match operand(0) {
                Value::Object(Some(obj)) => {
                    let mut ctx = NativeContextImpl { shared, thread };
                    if getter_driven.is_some() {
                        rec::record_hash_code_by_getters(
                            &mut ctx,
                            obj,
                            field_indices,
                            field_descriptors,
                        )?
                    } else {
                        rec::record_hash_code(&mut ctx, obj)?
                    }
                }
                // A null receiver (wave 39): the getters dereference it; with
                // none, `makeHashCode` is the constant 0.
                _ if !field_indices.is_empty() => return Err(record_null_receiver()),
                _ => 0,
            };
            Ok(Value::Int(hash))
        }
        RecordMethodKind::ToString => {
            let this = operand(0);
            // Built as UTF-16 code units and stored as a FRESH String, like
            // the concat path. This used to be a Rust `String` handed to
            // `create_java_string`, which INTERNS: every record `toString()`
            // result went into the process-wide string pool (unbounded growth
            // for a record logged in a loop), `r.toString() == r.toString()`
            // was `true` where HotSpot's is `false`, and a component carrying
            // a lone surrogate came out as U+FFFD.
            let units: Vec<u16> = match this {
                Value::Object(Some(obj)) => {
                    // `makeToString` names the record class the site was
                    // linked for, not the receiver's (a subclass of a
                    // non-`final` record; wave 40, lane L4). javac's shape is
                    // linked only for a `final` record
                    // (`Class::object_methods_args_are_canonical`), whose
                    // receiver IS that class.
                    let cid = getter_driven.unwrap_or_else(|| shared.mem.heap.class_id_of(obj));
                    let simple = record_class_simple_name(shared, cid);

                    // Reference components must render via their VIRTUAL
                    // toString (the JDK's generated record toString formats
                    // each component with String.valueOf) — the previous
                    // identity fallback printed `object@hash` for any non-String
                    // reference component (e.g. a List/Map/nested record),
                    // breaking record toString everywhere. The toString invoke
                    // can trigger a moving GC, so the record ref is pinned and
                    // re-read per component (mirrors the Equals/HashCode arms).
                    use cratonvm_native_api::NativeContext as _;
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    let obj_pin = ctx.pin_native_root(obj);
                    let mut result: Vec<u16> =
                        Vec::with_capacity(simple.len() + 2 + 16 * component_names.len());
                    result.extend(simple.encode_utf16());
                    result.push(u16::from(b'['));
                    let mut err: Option<MethodCallFailed> = None;
                    for (i, name) in component_names.iter().enumerate() {
                        if i > 0 {
                            result.extend(", ".encode_utf16());
                        }
                        let fi = field_indices.get(i).copied().unwrap_or(i);
                        let desc = field_descriptors.get(i).map(|s| &**s).unwrap_or("I");
                        let cur = ctx.read_native_pin(obj_pin, obj);
                        // An accessor-method getter (wave 41, `--real-jdk`):
                        // the handle `REF_invokeVirtual R.b()T` applied to
                        // the receiver. Its exception propagates, as the
                        // JDK's filtered getter's does. Selected from the
                        // receiver's class (which is the record class or a
                        // subclass: the site's type is `(R)String`), not by
                        // the record's name, which a loader-blind lookup
                        // could answer with another loader's class.
                        //
                        // Wave 42: a static getter `X.m(R)T` takes the
                        // receiver as its argument; a special getter
                        // `R.m()T` runs `R`'s method on it, never an
                        // override (`invokespecial` semantics).
                        let called = match accessor_getters.get(i) {
                            Some(Some(RecordAccessorGetter::Virtual { name, descriptor })) => {
                                Some(ctx.invoke_virtual(cur, name, descriptor, &[]))
                            }
                            Some(Some(RecordAccessorGetter::Static {
                                owner,
                                owner_name,
                                name,
                                descriptor,
                            })) => Some(ctx.invoke_static_by_class_id(
                                *owner,
                                owner_name,
                                name,
                                descriptor,
                                &[Value::Object(Some(cur))],
                            )),
                            Some(Some(RecordAccessorGetter::Special {
                                owner,
                                owner_name,
                                name,
                                descriptor,
                            })) => Some(ctx.invoke_special_by_class_id(
                                *owner,
                                owner_name,
                                name,
                                descriptor,
                                &[Value::Object(Some(cur))],
                            )),
                            _ => None,
                        };
                        let v = match called {
                            Some(Ok(Some(v))) => v,
                            Some(Ok(None)) => Value::Object(None),
                            Some(Err(e)) => {
                                err = Some(e);
                                break;
                            }
                            None => ctx.get_field(cur, fi),
                        };
                        result.extend(name.encode_utf16());
                        result.push(u16::from(b'='));
                        if let Err(e) =
                            append_record_component_units(&mut ctx, &v, desc, &mut result)
                        {
                            err = Some(e);
                            break;
                        }
                    }
                    ctx.unpin_native_roots(obj_pin);
                    if let Some(e) = err {
                        return Err(e);
                    }
                    result.push(u16::from(b']'));
                    result
                }
                // A null receiver (wave 39): the getters dereference it; with
                // none, `makeToString` is the constant `<simple name>[]`.
                _ if !field_indices.is_empty() => return Err(record_null_receiver()),
                _ => {
                    let record_class = getter_driven.unwrap_or(site_class_id);
                    let simple = record_class_simple_name(shared, record_class);
                    format!("{simple}[]").encode_utf16().collect()
                }
            };
            let str_ref = crate::runtime::interpreter::create_string_from_units_or_oom(
                shared, thread, &units,
            )?;
            Ok(Value::Object(Some(str_ref)))
        }
    }
}

/// `Class.getSimpleName()` of record class `cid`, as `makeToString` prints it:
/// [`record_simple_name`] over the class's OWN `InnerClasses` entry (only a
/// `$` in the binary name can make that differ from the last path segment),
/// `""` for an unknown class.
///
/// One class-manager read, one `String` (round 16 wave 9, lane indy). Every
/// record `toString()` used to clone the binary name and, through
/// `NativeContext::inner_classes`, all three strings of every `InnerClasses`
/// entry of the class, to keep one of them.
pub(in crate::runtime::invokedynamic) fn record_class_simple_name(shared: &SharedVm, cid: ClassId) -> String {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(cid) else {
        return String::new();
    };
    let binary: &str = &class.name;
    let inner_name = if binary.contains('$') {
        class
            .inner_classes
            .iter()
            .find(|ic| ic.inner_class == binary)
            .map(|ic| ic.inner_name.as_str())
    } else {
        None
    };
    record_simple_name(binary, inner_name).to_owned()
}

/// The `NullPointerException` a record `ObjectMethods` site raises for a null
/// receiver when it has getters (they dereference it). HotSpot's comes from
/// inside the getter's method handle; the probe compares only its class.
#[cold]
pub(in crate::runtime::invokedynamic) fn record_null_receiver() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NullPointerException {
        message: None,
    }))
}

/// `Class.getSimpleName()` of a record, from its binary name and the
/// `inner_name` of the record's OWN `InnerClasses` entry, if it has one.
///
/// HotSpot's rule (`Class.getSimpleName` → `getSimpleBinaryName`): a nested or
/// local class reports its `InnerClasses` `inner_name` verbatim (`Outer$Point`
/// and `Main$1Point` are both `Point`, and a member named `In$ner` stays
/// `In$ner`); a class with no entry of its own is TOP-LEVEL, and its simple
/// name is everything after the package, `$` included -- a top-level
/// `record A$B` is `A$B` (wave 24, lane L4; the old `$`-split answered `B`).
/// Without an entry the `$`-split is still used when the tail after the last
/// `$` starts with a digit (a local class whose `InnerClasses` could not be
/// read), since no Java identifier starts with one.
pub(in crate::runtime::invokedynamic) fn record_simple_name<'a>(binary_name: &'a str, inner_name: Option<&'a str>) -> &'a str {
    if let Some(inner) = inner_name.filter(|n| !n.is_empty()) {
        return inner;
    }
    let tail = binary_name.rsplit('/').next().unwrap_or(binary_name);
    let after = tail.rsplit('$').next().unwrap_or(tail);
    if after.starts_with(|c: char| c.is_ascii_digit()) {
        let stripped = after.trim_start_matches(|c: char| c.is_ascii_digit());
        if !stripped.is_empty() {
            return stripped;
        }
    }
    tail
}

/// The single UTF-16 unit of a boxed `java.lang.Character`, or `None` for
/// anything else.
///
/// Exists because that one wrapper is the only object whose whole value can be
/// an unpaired surrogate, which no `String`-returning renderer can carry.
pub(in crate::runtime::invokedynamic) fn boxed_char_unit(shared: &SharedVm, val: &Value) -> Option<u16> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    if shared.mem.heap.kind_of(*obj) == crate::memory::heap::ObjectKind::Array {
        return None;
    }
    if shared.mem.heap.get_header(*obj).num_slots() as usize != 1 {
        return None;
    }
    let name = shared
        .classes
        .class_manager
        .read()
        .get_class(shared.mem.heap.class_id_of(*obj))
        .map(|c| c.name.clone())
        .unwrap_or_default();
    if name.as_ref() != "java/lang/Character" {
        return None;
    }
    match shared.mem.heap.get_field(*obj, 0) {
        Value::Int(v) => Some(v as u16),
        _ => None,
    }
}

/// Render one record component like the JDK's generated record `toString`,
/// appending UTF-16 code units to `out`: primitives via their `String.valueOf`
/// form, references via `StringConcatHelper.stringOf` (`null` → "null", and a
/// `toString()` that returns null → "null"). The String content fast path
/// avoids a Java invoke for the common case. A `toString()` that throws
/// propagates.
pub(in crate::runtime::invokedynamic) fn append_record_component_units(
    ctx: &mut NativeContextImpl<'_>,
    v: &Value,
    descriptor: &str,
    out: &mut Vec<u16>,
) -> Result<(), MethodCallFailed> {
    match v {
        Value::Object(Some(obj)) => {
            // String fast path — copy the code units directly. Only for a
            // `String` by class (wave 16): the shape reader also answers for a
            // `StringBuilder` component, with its whole buffer (unused
            // capacity as NULs) where the JDK prints `String.valueOf(sb)`.
            if crate::vm::is_java_lang_string(ctx.shared, *obj) {
                if let Some(units) = read_java_string_units(&ctx.shared.mem.heap, *obj) {
                    out.extend_from_slice(&units);
                    return Ok(());
                }
            }
            // CALLED through `StringConcatHelper.stringOf(Object)`, as JDK 25's
            // `ObjectMethods.makeToString` concat chain does: a throwing
            // component `toString()` shows that frame between it and the record
            // frame (`R21RecnojitFrames` `*-two-tostring`, HotSpot 25).
            use cratonvm_native_api::NativeContext as _;
            match ctx.invoke_static(
                "java/lang/StringConcatHelper",
                "stringOf",
                "(Ljava/lang/Object;)Ljava/lang/String;",
                &[Value::Object(Some(*obj))],
            )? {
                Some(Value::Object(Some(s))) => {
                    match read_java_string_units(&ctx.shared.mem.heap, s) {
                        Some(units) => out.extend_from_slice(&units),
                        None => out.extend("null".encode_utf16()),
                    }
                }
                // toString returned null (legal) → JDK prints "null".
                _ => out.extend("null".encode_utf16()),
            }
        }
        // A `char` component is one code unit, surrogate or not.
        Value::Int(n) if descriptor == "C" => out.push(*n as u16),
        // Primitives and the null reference: descriptor-aware textual form.
        _ => out.extend(format_field_value(ctx.shared, v, descriptor).encode_utf16()),
    }
    Ok(())
}

/// Format a field value for record toString.
///
/// `float`/`double` go through the `Float.toString`/`Double.toString`
/// formatters. They used Rust's `{}`, which prints `1.0f` as `1`, `1e10` as
/// `10000000000` and `-0.0` as `-0`, so `record P(double x)` with `x = 1.0`
/// rendered `P[x=1]` where HotSpot prints `P[x=1.0]`.
pub(in crate::runtime::invokedynamic) fn format_field_value(shared: &SharedVm, v: &Value, descriptor: &str) -> String {
    match v {
        Value::Int(n) => {
            if descriptor == "Z" {
                if *n != 0 {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            } else if descriptor == "C" {
                format!("{}", char::from_u32(*n as u32).unwrap_or('?'))
            } else {
                n.to_string()
            }
        }
        Value::Long(n) => n.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Double(d) => format_double(*d),
        Value::Object(Some(obj)) => {
            if let Some(s) = read_java_string(&shared.mem.heap, *obj) {
                s
            } else {
                format!("object@{:x}", obj.as_ptr() as usize)
            }
        }
        Value::Object(None) => "null".to_string(),
        _ => "?".to_string(),
    }
}

/// Bootstrap a `SwitchBootstraps.enumSwitch` call site.
///
/// Bootstrap arguments are *"String constants and Class instances, in any
/// combination"*: an enum constant name, or a type (javac emits one for a
/// type pattern such as `case E x when ...` beside constant labels). The
/// resulting call site takes `(Enum target, int startIndex)` and returns the
/// index of the first matching label from `startIndex` on.
///
/// An all-name switch is linked as [`ResolvedCallSite::EnumSwitch`]. One with
/// a `Class` label is linked as a `TypeSwitch` of `EnumConstant` and `Type`
/// labels, so the type label answers `instanceof` exactly as `typeSwitch`
/// does. A `Class` label used to be read as a string constant, i.e. `""`,
/// which no constant name equals, so the pattern arm was never taken.
pub(in crate::runtime::invokedynamic) fn bootstrap_enum_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Phase 1, under the read lock: every label as a name, or as a class name
    // to resolve once the lock is released.
    let raw: Vec<Result<Arc<str>, Arc<str>>> = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("enumSwitch: class {current_class_id} not found"),
            })?;
        let cp = &class.constant_pool;
        info.bootstrap_arg_indices
            .iter()
            .map(|&arg_idx| match cp.get(arg_idx) {
                Some(ConstantPoolEntry::ClassReference { .. }) => {
                    Err(Arc::<str>::from(cp.get_class_name(arg_idx).unwrap_or("")))
                }
                // Anything else keeps the old reading: a name (`""` for an
                // entry that is not a string, which matches no constant).
                _ => Ok(Arc::<str>::from(
                    resolve_string_constant(cp, arg_idx).unwrap_or_default(),
                )),
            })
            .collect()
    };

    if raw.iter().any(Result::is_err) {
        // Phase 2: resolve each `Class` label through the switching class's
        // loader, as `bootstrap_type_switch` does.
        let mut labels = Vec::with_capacity(raw.len());
        for label in raw {
            labels.push(match label {
                Ok(name) => SwitchLabel::EnumConstant(name),
                Err(class_name) => SwitchLabel::Type {
                    class_id: crate::runtime::interpreter::resolve_class_loader_aware(
                        shared,
                        thread,
                        current_class_id,
                        &class_name,
                    )
                    .map_err(|e| {
                        crate::runtime::exceptions::convert_class_not_found(
                            shared,
                            thread,
                            &class_name,
                            e,
                        )
                    })?,
                    class_name,
                },
            });
        }
        let site = ResolvedCallSite::TypeSwitch {
            labels: labels.clone(),
        };
        shared.classes.resolution_cache.write().put_call_site_as_of(
            current_class_id,
            cp_index,
            site,
            info.fill_as_of,
        );
        return execute_type_switch(shared, thread, frame_idx, &labels);
    }
    let labels: Vec<Arc<str>> = raw.into_iter().flatten().collect();

    // Cache the call site.
    let site = ResolvedCallSite::EnumSwitch {
        labels: labels.clone(),
    };
    shared.classes.resolution_cache.write().put_call_site_as_of(
        current_class_id,
        cp_index,
        site,
        info.fill_as_of,
    );

    execute_enum_switch(shared, thread, frame_idx, &labels)
}

/// Execute a cached `enumSwitch` call site.
///
/// Stack: `[..., target: Enum, startIndex: int]` → `[..., matchIndex: int]`
pub fn execute_enum_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    labels: &[Arc<str>],
) -> Result<(), MethodCallFailed> {
    let restart = thread.frames[frame_idx].stack.pop()?;
    let target = thread.frames[frame_idx].stack.pop()?;
    let result = enum_switch_index(shared, target, restart, labels)?;
    thread.frames[frame_idx].stack.push(Value::Int(result))?;
    Ok(())
}

/// [`execute_enum_switch`] without a frame -- see [`type_switch_index`].
pub(in crate::runtime::invokedynamic) fn enum_switch_index(
    shared: &SharedVm,
    target: Value,
    restart: Value,
    labels: &[Arc<str>],
) -> Result<i32, MethodCallFailed> {
    let restart = match restart {
        Value::Int(i) => i,
        _ => 0,
    };
    let start_index = restart.max(0) as usize;

    let result = match target {
        // Per the `SwitchBootstraps.enumSwitch` contract, a null target returns
        // -1 (the `case null` arm); a non-null target that matches no label
        // returns labels.length (the default arm).
        Value::Object(None) => -1,
        // A restart index outside `0..=labels.length` (wave 42): the JDK's
        // `mappedEnumSwitch` answers a null first, then its generated switch
        // makes `Objects.checkIndex(restart, labels.length + 1)`. It was
        // clamped to 0, and a large one answered the default.
        Value::Object(Some(_)) if restart < 0 || restart as usize > labels.len() => {
            return Err(switch_restart_out_of_bounds(restart, labels.len()));
        }
        Value::Object(Some(obj_ref)) => {
            // Read the enum constant name from field 0 (Enum.<init> stores name there).
            let name = match shared.mem.heap.get_field(obj_ref, 0) {
                Value::Object(Some(name_ref)) => read_java_string(&shared.mem.heap, name_ref),
                _ => None,
            };

            let mut match_index: i32 = labels.len() as i32;
            if let Some(name) = name {
                for (i, label) in labels.iter().enumerate().skip(start_index) {
                    if &**label == name.as_str() {
                        match_index = i as i32;
                        break;                    }                }            }
            match_index        }
        _ => labels.len() as i32,
    };
    Ok(result)
}

