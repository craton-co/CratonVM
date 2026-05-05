//! Throwable, StackTraceElement, Enum, and Record native method implementations.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectRef, Value};
use rustjvm_types::error::MethodCallResult;

use crate::obj_arg;

/// Helper: write Throwable.detailMessage on a Throwable subclass.
///
/// In real-JDK Throwable, detailMessage is at slot 1 (after backtrace at
/// slot 0). In our synthetic-stub layout, it's at slot 0 (the stubs use
/// unnamed `_f0`, `_f1`). Use `set_field_by_name` to honour the real-JDK
/// layout when present, then mirror to slot 0 so synthetic-stub code paths
/// (which read slot 0 directly) still observe the message.
fn write_throwable_detail_message(ctx: &mut dyn NativeContext, this: ObjectRef, msg: Value) {
    ctx.set_field_by_name(this, "detailMessage", msg);
    ctx.set_field(this, 0, msg);
}

/// Helper: write Throwable.cause on a Throwable subclass.
///
/// Mirrors `write_throwable_detail_message`: by-name first (real-JDK slot
/// is `cause` at slot 2 after backtrace+detailMessage), then slot 1 to
/// keep the synthetic-stub layout in sync.
fn write_throwable_cause(ctx: &mut dyn NativeContext, this: ObjectRef, cause: Value) {
    ctx.set_field_by_name(this, "cause", cause);
    ctx.set_field(this, 1, cause);
}

/// Exception <init>(Ljava/lang/String;)V — sets detailMessage.
pub(crate) fn native_exc_init_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let (Some(Value::Object(Some(this))), Some(msg)) = (args.first(), args.get(1)) {
        write_throwable_detail_message(ctx, *this, *msg);
    }
    Ok(None)
}

/// Exception <init>(Ljava/lang/String;Ljava/lang/Throwable;)V
pub(crate) fn native_exc_init_message_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(msg) = args.get(1) {
            write_throwable_detail_message(ctx, *this, *msg);
        }
        if let Some(cause) = args.get(2) {
            write_throwable_cause(ctx, *this, *cause);
        }
    }
    Ok(None)
}

/// Exception <init>(Ljava/lang/Throwable;)V — sets cause.
pub(crate) fn native_exc_init_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let (Some(Value::Object(Some(this))), Some(cause)) = (args.first(), args.get(1)) {
        write_throwable_cause(ctx, *this, *cause);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// java.lang.Throwable natives
// ---------------------------------------------------------------------------

pub(crate) fn native_throwable_fill_in_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (the Throwable), args[1] = dummy int
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("fillInStackTrace on null".to_string()),
            }
            .into());
        }
    };

    let hash = ctx.identity_hash_code(this);
    let _trace = ctx.capture_stack_trace(hash);

    // Return `this` (Throwable.fillInStackTrace returns the Throwable itself)
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_throwable_get_stack_trace_depth(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => return Ok(Some(Value::Int(0))),
    };

    let hash = ctx.identity_hash_code(this);
    let depth = ctx
        .get_stack_trace(hash)
        .map_or(0, |trace| trace.len() as i32);
    Ok(Some(Value::Int(depth)))
}

/// getStackTraceElement(int index) — return a StackTraceElement for the given frame index.
///
/// Creates a StackTraceElement object with declaringClass, methodName, fileName, lineNumber.
pub(crate) fn native_throwable_get_stack_trace_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("getStackTraceElement on null".to_string()),
            }
            .into());
        }
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };

    let hash = ctx.identity_hash_code(this);

    // Clone the trace entry data to release the immutable borrow on ctx
    // before we need mutable access for object allocation.
    let entry_data = ctx
        .get_stack_trace(hash)
        .and_then(|t| t.get(index as usize))
        .cloned();

    // Helper: build a StackTraceElement with 4 fields
    let build_ste = |ctx: &mut dyn NativeContext,
                     class_name: &str,
                     method_name: &str,
                     file_name: Option<&str>,
                     line: i32|
     -> ObjectRef {
        let ste_obj = ctx.alloc_object(rustjvm_types::ClassId::new(0), 4);
        let cs = ctx.create_string(class_name);
        ctx.set_field(ste_obj, 0, Value::Object(Some(cs)));
        let ms = ctx.create_string(method_name);
        ctx.set_field(ste_obj, 1, Value::Object(Some(ms)));
        if let Some(f) = file_name {
            let fs = ctx.create_string(f);
            ctx.set_field(ste_obj, 2, Value::Object(Some(fs)));
        } else {
            ctx.set_field(ste_obj, 2, Value::Object(None));
        }
        ctx.set_field(ste_obj, 3, Value::Int(line));
        ste_obj
    };

    match entry_data {
        Some(ste) => {
            let obj = build_ste(
                ctx,
                &ste.class_name,
                &ste.method_name,
                ste.source_file.as_deref(),
                ste.line_number,
            );
            Ok(Some(Value::Object(Some(obj))))
        }
        None => {
            let obj = build_ste(ctx, "<unknown>", "<unknown>", None, -1);
            Ok(Some(Value::Object(Some(obj))))
        }
    }
}

pub(crate) fn native_throwable_get_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real-JDK Throwable layout has `detailMessage` at slot 1 (after
    // `backtrace` at slot 0); synthetic-stub layout puts it at slot 0
    // (unnamed `_f0`). Prefer the field-name lookup so the real-JDK
    // bytecode (which writes via `putfield detailMessage`) and our
    // native init helpers (which now mirror to both) agree.
    let by_name = ctx.get_field_by_name(this, "detailMessage");
    let detail = match by_name {
        Value::Object(Some(_)) => by_name,
        // Fallback: synthetic-stub layout where the field has no `detailMessage`
        // name and the canonical slot is index 0.
        _ => ctx.get_field(this, 0),
    };
    // Return the field value directly. The previous `read_string` validation
    // dropped legitimate JDK String references whose internal layout
    // `read_java_string` couldn't parse during early boot — for the common
    // `Throwable(String)` ctor the field holds a real `java/lang/String`
    // and the caller treats the returned reference as one regardless.
    match detail {
        Value::Object(obj_opt) => Ok(Some(Value::Object(obj_opt))),
        _ => Ok(Some(Value::Object(None))),
    }
}

// --- Throwable additional methods ---

/// getCause() — read the cause field.
///
/// Real-JDK Throwable has `cause` at slot 2 (after backtrace, detailMessage).
/// Our synthetic-stub layout puts it at slot 1 (unnamed `_f1`). Prefer the
/// field-name lookup so this works regardless of layout.
///
/// `InvocationTargetException` (and similar wrappers) override `getCause()`
/// in bytecode to return their own field (`target`). When the dispatch path
/// routes here anyway — e.g. via the Throwable-base hierarchy walk after a
/// stale invoke-cache miss — we mimic the override by reading `target` when
/// the synthesized cause field is null. The check is field-name-driven so
/// it stays layout-agnostic.
///
/// JDK-spec sentinel: real-JDK `Throwable` declares `private Throwable
/// cause = this;` — a self-reference means "no cause set yet" — and
/// `getCause()` returns `null` in that case. Real-JDK bytecode running
/// through our interpreter (and any path that mirrors that initializer)
/// can therefore land here with `cause == this`. Returning `this` would
/// drive Spring Boot's `getExitCodeFromExitCodeGeneratorException`
/// recursion into a `StackOverflowError`. Map the sentinel to `null` per
/// the JDK contract.
pub(crate) fn native_throwable_get_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let by_name_cause = ctx.get_field_by_name(this, "cause");
    if let Value::Object(Some(cause_obj)) = by_name_cause {
        // JDK sentinel: cause==this means "uninitialized cause"; report null.
        if cause_obj == this {
            return Ok(Some(Value::Object(None)));
        }
        return Ok(Some(by_name_cause));
    }
    // ITE / other wrappers: getCause() returns the dedicated `target`
    // field, not the inherited Throwable.cause. Mirror that here so the
    // wrapper exception propagates the correct cause to JLS-spec callers.
    let by_name_target = ctx.get_field_by_name(this, "target");
    if let Value::Object(Some(target_obj)) = by_name_target {
        // Same self-reference guard, in case any wrapper's bytecode
        // initializes its `target` field with `this` as a sentinel.
        if target_obj == this {
            return Ok(Some(Value::Object(None)));
        }
        return Ok(Some(by_name_target));
    }
    // Synthetic-stub fallback: layout has no named `cause` but reserves
    // slot 1 for the cause reference.
    let slot1 = ctx.get_field(this, 1);
    match slot1 {
        Value::Object(Some(obj)) if obj == this => Ok(Some(Value::Object(None))),
        Value::Object(obj_opt) => Ok(Some(Value::Object(obj_opt))),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// initCause(Throwable) — set the cause field, return this.
pub(crate) fn native_throwable_init_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cause_val = args.get(1).cloned().unwrap_or(Value::Object(None));
    write_throwable_cause(ctx, this, cause_val);
    Ok(Some(Value::Object(Some(this))))
}

/// toString() — build "ClassName: message" or just "ClassName"
pub(crate) fn native_throwable_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Get the class name
    let class_id = ctx.class_id_of_object(this);
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "java/lang/Throwable".to_string())
        .replace('/', ".");

    // Read detailMessage by name first (real-JDK Throwable layout has it
    // at slot 1), with slot-0 fallback for synthetic stubs.
    let by_name = ctx.get_field_by_name(this, "detailMessage");
    let detail = match by_name {
        Value::Object(Some(_)) => by_name,
        _ => ctx.get_field(this, 0),
    };
    let result = match detail {
        Value::Object(Some(str_ref)) => {
            if let Some(msg) = ctx.read_string(str_ref) {
                format!("{class_name}: {msg}")
            } else {
                class_name
            }
        }
        _ => class_name,
    };

    let str_obj = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// printStackTrace() — print class name + message to output
pub(crate) fn native_throwable_print_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };

    // Get the class name
    let class_id = ctx.class_id_of_object(this);
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "java/lang/Throwable".to_string())
        .replace('/', ".");

    // Get the message (field 0)
    let detail = ctx.get_field(this, 0);
    let header = match detail {
        Value::Object(Some(str_ref)) => {
            if let Some(msg) = ctx.read_string(str_ref) {
                format!("{class_name}: {msg}")
            } else {
                class_name
            }
        }
        _ => class_name,
    };

    ctx.record_printed_line(header);

    // Print cause chain
    let cause = ctx.get_field(this, 1);
    if let Value::Object(Some(cause_ref)) = cause {
        // Avoid self-referential cause
        if !std::ptr::eq(cause_ref.as_ptr(), this.as_ptr()) {
            let cause_class_id = ctx.class_id_of_object(cause_ref);
            let cause_class_name = ctx
                .class_name_of_id(cause_class_id)
                .unwrap_or_else(|| "?".to_string())
                .replace('/', ".");
            let cause_detail = ctx.get_field(cause_ref, 0);
            let cause_line = match cause_detail {
                Value::Object(Some(sr)) => {
                    if let Some(m) = ctx.read_string(sr) {
                        format!("Caused by: {cause_class_name}: {m}")
                    } else {
                        format!("Caused by: {cause_class_name}")
                    }
                }
                _ => format!("Caused by: {cause_class_name}"),
            };
            ctx.record_printed_line(cause_line);
        }
    }

    Ok(None)
}

/// addSuppressed(Throwable) — append to suppressed list stored in field 2
pub(crate) fn native_throwable_add_suppressed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use rustjvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let suppressed = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Don't allow self-suppression
    if this == suppressed {
        return Ok(None);
    }
    // Check if the object has enough fields for suppressed storage (field 2)
    let num_fields = ctx.object_num_fields(this);
    if num_fields < 3 {
        return Ok(None); // object too small, silently ignore
    }
    // Get existing suppressed array from field 2
    let existing = ctx.get_field(this, 2);
    match existing {
        Value::Object(Some(arr)) => {
            // Grow the array: copy old elements + append new one
            let old_len = ctx.array_length(arr);
            let new_arr = ctx.new_ref_array(ClassId::new(0), old_len + 1);
            for i in 0..old_len {
                let elem = ctx.get_array_element(arr, i);
                ctx.set_array_element(new_arr, i, elem);
            }
            ctx.set_array_element(new_arr, old_len, Value::Object(Some(suppressed)));
            ctx.set_field(this, 2, Value::Object(Some(new_arr)));
        }
        _ => {
            // No existing array — create one with single element
            let new_arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(new_arr, 0, Value::Object(Some(suppressed)));
            ctx.set_field(this, 2, Value::Object(Some(new_arr)));
        }
    }
    Ok(None)
}

/// getSuppressed() — return Throwable[] from field 2, or empty array if not set
pub(crate) fn native_throwable_get_suppressed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use rustjvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let num_fields = ctx.object_num_fields(this);
    if num_fields >= 3 {
        if let Value::Object(Some(arr)) = ctx.get_field(this, 2) {
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // No suppressed exceptions stored — return empty array
    let arr = ctx.new_ref_array(ClassId::new(0), 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// getStackTrace() — return StackTraceElement[] from captured stack trace
pub(crate) fn native_throwable_get_stack_trace_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use rustjvm_types::ClassId;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let hash = ctx.identity_hash_code(this);
    // Clone trace data to avoid borrow conflict with ctx
    let trace_data: Vec<_> = ctx
        .get_stack_trace(hash)
        .map(|t| {
            t.iter()
                .map(|e| {
                    (
                        e.class_name.replace('/', "."),
                        e.method_name.to_string(),
                        e.source_file.as_ref().map(|f| f.to_string()),
                        e.line_number,
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let len = trace_data.len();
    let arr = ctx.new_ref_array(ClassId::new(0), len);
    for (i, (cls, meth, file, line)) in trace_data.iter().enumerate() {
        let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
        let cls_str = ctx.create_string(cls);
        let meth_str = ctx.create_string(meth);
        ctx.set_field(ste, 0, Value::Object(Some(cls_str)));
        ctx.set_field(ste, 1, Value::Object(Some(meth_str)));
        if let Some(f) = file {
            let file_str = ctx.create_string(f);
            ctx.set_field(ste, 2, Value::Object(Some(file_str)));
        } else {
            ctx.set_field(ste, 2, Value::Object(None));
        }
        ctx.set_field(ste, 3, Value::Int(*line));
        ctx.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ===========================================================================
// java.lang.Enum
// ===========================================================================

pub(crate) fn register_enum_natives(r: &mut NativeMethodRegistry) {
    let e = "java/lang/Enum";
    r.register(e, "<init>", "(Ljava/lang/String;I)V", native_enum_init);
    r.register(e, "ordinal", "()I", native_enum_ordinal);
    r.register(e, "name", "()Ljava/lang/String;", native_enum_name);
    r.register(e, "toString", "()Ljava/lang/String;", native_enum_name); // delegates to name()
    r.register(
        e,
        "compareTo",
        "(Ljava/lang/Enum;)I",
        native_enum_compare_to,
    );
    r.register(e, "equals", "(Ljava/lang/Object;)Z", native_enum_equals);
    r.register(e, "hashCode", "()I", native_enum_hash_code);
    r.register(
        e,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        native_enum_get_declaring_class,
    );
}

/// Enum.<init>(String name, int ordinal) — store name in field 0, ordinal in field 1
pub(crate) fn native_enum_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [this, name(String ref), ordinal(int)]
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let name_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let ordinal = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    ctx.set_field(this, 0, name_val);
    ctx.set_field(this, 1, Value::Int(ordinal));
    Ok(None)
}

/// Enum.ordinal() — return field 1 (int)
pub(crate) fn native_enum_ordinal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, 1)))
}

/// Enum.name() and Enum.toString() — return field 0 (String ref)
pub(crate) fn native_enum_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

/// Enum.compareTo(Enum other) — this.ordinal - other.ordinal
pub(crate) fn native_enum_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_ord = match ctx.get_field(this, 1) {
        Value::Int(i) => i,
        _ => 0,
    };
    let other_ord = match ctx.get_field(other, 1) {
        Value::Int(i) => i,
        _ => 0,
    };
    Ok(Some(Value::Int(this_ord - other_ord)))
}

/// Enum.equals(Object) — identity comparison (reference equality)
pub(crate) fn native_enum_equals(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    let eq = match (this, other) {
        (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
        _ => false,
    };
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))
}

/// Enum.hashCode() — identity hash code
pub(crate) fn native_enum_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.identity_hash_code(this))))
}

/// Enum.getDeclaringClass() — return Class mirror for this enum's class
pub(crate) fn native_enum_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = ctx.class_id_of_object(this);
    let mirror = ctx.get_class_mirror(class_id);
    Ok(Some(Value::Object(Some(mirror))))
}

// --- StackTraceElement ---
const STE_FIELD_CLASS: usize = 0;
const STE_FIELD_METHOD: usize = 1;
const STE_FIELD_FILE: usize = 2;
const STE_FIELD_LINE: usize = 3;

pub(crate) fn native_ste_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(
        this,
        STE_FIELD_CLASS,
        args.get(1).copied().unwrap_or(Value::Object(None)),
    );
    ctx.set_field(
        this,
        STE_FIELD_METHOD,
        args.get(2).copied().unwrap_or(Value::Object(None)),
    );
    ctx.set_field(
        this,
        STE_FIELD_FILE,
        args.get(3).copied().unwrap_or(Value::Object(None)),
    );
    ctx.set_field(
        this,
        STE_FIELD_LINE,
        args.get(4).copied().unwrap_or(Value::Int(-1)),
    );
    Ok(None)
}

pub(crate) fn native_ste_get_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, STE_FIELD_CLASS)))
}

pub(crate) fn native_ste_get_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, STE_FIELD_METHOD)))
}

pub(crate) fn native_ste_get_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, STE_FIELD_FILE)))
}

pub(crate) fn native_ste_get_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(ctx.get_field(this, STE_FIELD_LINE)))
}

pub(crate) fn native_ste_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class = match ctx.get_field(this, STE_FIELD_CLASS) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => "Unknown".to_string(),
    };
    let method = match ctx.get_field(this, STE_FIELD_METHOD) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => "unknown".to_string(),
    };
    let file = match ctx.get_field(this, STE_FIELD_FILE) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => "Unknown Source".to_string(),
    };
    let line = match ctx.get_field(this, STE_FIELD_LINE) {
        Value::Int(v) => v,
        _ => -1,
    };
    let s = if line >= 0 {
        format!("{}.{}({}:{})", class, method, file, line)
    } else {
        format!("{}.{}({})", class, method, file)
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn register_phase53_record(r: &mut NativeMethodRegistry) {
    let rec = "java/lang/Record";
    // Records are just normal classes with some special semantics
    // We register equals/hashCode/toString stubs that work via fields.
    //
    // S107 fix (constructor_probe test 10): the no-arg `<init>()V` is void,
    // so it must return `Ok(None)`. Returning `Ok(Some(Value::Object(None)))`
    // pushes a stray null onto the operand stack, which silently corrupts
    // the caller record's `<init>` frame — for compact-canonical records
    // with a validation body, this can shift max_stack and cause the
    // throw branch to be skipped or mis-dispatched. See WP2.6.
    r.register(rec, "<init>", "()V", |_ctx, _args| {
        Ok(None)
    });
    r.register(rec, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            // Records: must be same class, not just same field count
            let this_cid = ctx.class_id_of_object(this);
            let other_cid = ctx.class_id_of_object(*other);
            if this_cid != other_cid {
                return Ok(Some(Value::Int(0)));
            }
            let nf = ctx.object_num_fields(this);
            for i in 0..nf {
                if ctx.get_field(this, i) != ctx.get_field(*other, i) {
                    return Ok(Some(Value::Int(0)));
                }
            }
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(rec, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let nf = ctx.object_num_fields(this);
        let mut hash: i32 = 0;
        for i in 0..nf {
            let v = ctx.get_field(this, i);
            let h = match v {
                Value::Int(n) => n,
                Value::Long(n) => (n ^ (n >> 32)) as i32,
                Value::Float(f) => f.to_bits() as i32,
                Value::Double(d) => {
                    let bits = d.to_bits();
                    (bits ^ (bits >> 32)) as i32
                }
                Value::Object(Some(r)) => r.as_ptr() as i32,
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(h);
        }
        Ok(Some(Value::Int(hash)))
    });
    r.register(rec, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cid = ctx.class_id_of_object(this);
        let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
        // Use simple name (after last '/')
        let simple = class_name.rsplit('/').next().unwrap_or(&class_name);
        let components = ctx.record_components(cid);
        if components.is_empty() {
            // Fallback for non-record or unknown
            let s = ctx.create_string(&format!("{}@{:x}", simple, this.as_ptr() as usize));
            return Ok(Some(Value::Object(Some(s))));
        }
        let mut result = format!("{}[", simple);
        for (i, (name, descriptor)) in components.iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            let val = ctx.get_field(this, i);
            let val_str = match val {
                Value::Int(n) => {
                    if descriptor == "Z" {
                        if n != 0 {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    } else if descriptor == "C" {
                        format!("{}", char::from_u32(n as u32).unwrap_or('?'))
                    } else {
                        n.to_string()
                    }
                }
                Value::Long(n) => n.to_string(),
                Value::Float(f) => f.to_string(),
                Value::Double(d) => d.to_string(),
                Value::Object(Some(obj)) => ctx
                    .read_string(obj)
                    .unwrap_or_else(|| format!("object@{:x}", obj.as_ptr() as usize)),
                Value::Object(None) => "null".to_string(),
                _ => "?".to_string(),
            };
            result.push_str(&format!("{}={}", name, val_str));
        }
        result.push(']');
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    });
}

// =============================================================================
// java.lang.Record expansion — components, equals, hashCode, toString stubs
// =============================================================================

pub(crate) fn register_p60_record(r: &mut NativeMethodRegistry) {
    // Record equals/hashCode/toString already registered in earlier phase with proper
    // field-by-field comparison — only add RecordComponent here.

    // RecordComponent = 3-field (name=0, type=1, declaringRecord=2)
    let rc = "java/lang/reflect/RecordComponent";
    r.register(rc, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(rc, "getType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        rc,
        "getDeclaringRecord",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
}

// =============================================================================
// WP8.10.7 — Throwable.getMessage / printStackTrace on synthetic-stub
// Throwable subclasses
// =============================================================================
//
// Background: `getMessage()`, `getLocalizedMessage()` and `printStackTrace()`
// (both no-arg and `(PrintStream)V`) are declared on `java/lang/Throwable`,
// but the registry dispatch is keyed by class name — so a `catch (Throwable t) {
// t.getMessage(); }` whose `t` is a `NoClassDefFoundError` synthetic-stub will
// surface a secondary `NoSuchMethodError` because no native is registered under
// `java/lang/NoClassDefFoundError.getMessage`.
//
// Field layout (verified against
// `classloading/src/class_manager.rs::synthetic_stub_fields` arm at line 3373+):
//   slot 0 = detailMessage (String)
//   slot 1 = cause (Throwable)
// Each subclass is allocated with `instance_fields(2)` so the slot indexing
// is robust as long as that arm is not re-ordered.
//
// We intentionally re-register `java/lang/Throwable` itself too so the
// behavior is uniform across the family — `register` is last-write-wins
// and the new closure is a strict superset of the existing native (it
// reads slot 0, validates it's a string, and falls back to null).
pub fn register_throwable_subclass_natives(r: &mut NativeMethodRegistry) {
    // Subset that surfaces in jboss-modules / WildFly catch-blocks (per
    // bench/wildfly-boot/diagnostic.md §WP8.10.7) plus the broader
    // Error/Exception families that any defensive catch will see.
    let throwable_classes = [
        "java/lang/Throwable",
        "java/lang/Exception",
        "java/lang/RuntimeException",
        "java/lang/Error",
        "java/lang/LinkageError",
        "java/lang/NoClassDefFoundError",
        "java/lang/ClassNotFoundException",
        "java/lang/NoSuchMethodError",
        "java/lang/NoSuchFieldError",
        "java/lang/NoSuchMethodException",
        "java/lang/NoSuchFieldException",
        "java/lang/NullPointerException",
        "java/lang/ArithmeticException",
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/IndexOutOfBoundsException",
        "java/lang/ClassCastException",
        "java/lang/IllegalArgumentException",
        "java/lang/IllegalStateException",
        "java/lang/UnsupportedOperationException",
        "java/lang/StackOverflowError",
        "java/lang/OutOfMemoryError",
        "java/util/NoSuchElementException",
        "java/util/InputMismatchException",
        "java/io/IOException",
        "java/io/FileNotFoundException",
        "java/lang/NumberFormatException",
        "java/util/ConcurrentModificationException",
        "java/lang/NegativeArraySizeException",
        "java/lang/AssertionError",
        "java/lang/MatchException",
        "java/lang/IncompatibleClassChangeError",
        "java/lang/ExceptionInInitializerError",
        "java/lang/VerifyError",
        "java/lang/AbstractMethodError",
        "java/lang/InternalError",
        "java/lang/UnsatisfiedLinkError",
    ];

    for cls in throwable_classes.iter() {
        // getMessage()Ljava/lang/String; — read slot 0 (detailMessage).
        r.register(
            cls,
            "getMessage",
            "()Ljava/lang/String;",
            native_throwable_get_message,
        );
        // getLocalizedMessage()Ljava/lang/String; — JDK delegates to
        // getMessage by default.
        r.register(
            cls,
            "getLocalizedMessage",
            "()Ljava/lang/String;",
            native_throwable_get_message,
        );
        // printStackTrace()V — no-arg overload, prints to System.err equivalent.
        r.register(
            cls,
            "printStackTrace",
            "()V",
            native_throwable_print_stack_trace,
        );
        // printStackTrace(Ljava/io/PrintStream;)V — JDK 25 overload.
        r.register(
            cls,
            "printStackTrace",
            "(Ljava/io/PrintStream;)V",
            native_throwable_print_stack_trace_to_stream,
        );
        // printStackTrace(Ljava/io/PrintWriter;)V — same shape, same sink.
        r.register(
            cls,
            "printStackTrace",
            "(Ljava/io/PrintWriter;)V",
            native_throwable_print_stack_trace_to_stream,
        );
        // toString()Ljava/lang/String; — "ClassName: message".
        r.register(
            cls,
            "toString",
            "()Ljava/lang/String;",
            native_throwable_to_string,
        );
        // getCause()Ljava/lang/Throwable; — read slot 1.
        r.register(
            cls,
            "getCause",
            "()Ljava/lang/Throwable;",
            native_throwable_get_cause,
        );
    }
}

/// printStackTrace(Ljava/io/PrintStream;)V (and PrintWriter overload).
///
/// Args layout: [this, stream]. We reuse the no-arg printStackTrace
/// implementation which writes to the recorded-line sink; the
/// PrintStream/PrintWriter argument is intentionally ignored because
/// the recorded-line sink is already wired through to System.err in
/// the lib.rs PrintStream natives.
///
/// Null-safe: if `this` is null we no-op; if `stream` is null we still
/// print, because catch-block code paths frequently call
/// `t.printStackTrace(System.err)` and the synthetic-stub System.err
/// static may itself be null on early boot — we don't want to throw a
/// secondary NPE inside a catch handler.
pub(crate) fn native_throwable_print_stack_trace_to_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_throwable_print_stack_trace(ctx, args)
}

