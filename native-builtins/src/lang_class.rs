//! Class, reflect.Method, reflect.Field, reflect.Constructor native method implementations.

use rustjvm_native_api::{FieldMetadata, MethodMetadata, NativeContext};
use rustjvm_types::{ClassId, ObjectRef, Value};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult};

use crate::obj_arg;
use crate::lang_math::alloc_wrapper;
use crate::alloc_concurrent_synthetic;

// ---------------------------------------------------------------------------
// Access control constants (JVM access flags)
//
// Canonical definitions live in `rustjvm_types::access_flags`; the `_I32`
// aliases here keep existing call sites compact while pointing at the
// shared source of truth.
// ---------------------------------------------------------------------------

use rustjvm_types::access_flags::{
    ACC_PUBLIC_I32 as ACC_PUBLIC,
    ACC_STATIC_I32 as ACC_STATIC,
    ACC_FINAL_I32 as ACC_FINAL,
    ACC_VOLATILE_I32 as ACC_VOLATILE,
};

/// WP2.1-field — final-field write check for `Field.set*`.
///
/// Per `java.lang.reflect.Field.set` Javadoc and JLS §15.26.1:
///   * Writing a non-static `final` field via reflection requires
///     `setAccessible(true)`. Without it, throws IllegalAccessException.
///   * Writing a `static final` field is **always** disallowed via the
///     plain `Field.set*` path — even with `setAccessible(true)`. (The
///     escape hatch is `Unsafe.staticFieldBase`/`putReference` or
///     `MethodHandles.Lookup.findStaticVarHandle`, neither of which
///     route through `Field.set`.)
///   * Records, hidden classes, and `enum` constants are likewise pinned
///     via final fields; we treat them under the same rule. The
///     fine-grained record/hidden-class differentiation belongs in a
///     follow-up — for now we conservatively reject the write.
fn check_final_for_set(
    modifiers: i32,
    accessible: bool,
    member_desc: &str,
) -> Result<(), rustjvm_types::error::MethodCallFailed> {
    if (modifiers & ACC_FINAL) == 0 {
        return Ok(());
    }
    let is_static = (modifiers & ACC_STATIC) != 0;
    // Static-final: hard-disallowed regardless of `setAccessible`.
    if is_static {
        return Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
            message: format!(
                "Can not set static final field via Field.set: {}",
                member_desc,
            ),
        }
        .into());
    }
    // Instance-final: requires `setAccessible(true)`.
    if !accessible {
        return Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
            message: format!(
                "Can not set final field without setAccessible(true): {}",
                member_desc,
            ),
        }
        .into());
    }
    Ok(())
}

/// WP2.1-field — volatile-aware load barrier.
///
/// `Field.get` / `Field.getInt` / `Field.getLong` etc. observe the
/// volatile read semantics of the field, which on the JDK delegate
/// internally to `Unsafe.getReferenceVolatile` / `getIntVolatile` /
/// `getLongVolatile`. We mirror that by issuing an `Acquire` fence
/// before the load when the field is `ACC_VOLATILE`. Non-volatile
/// fields skip the fence to avoid the perf hit on plain reads.
fn volatile_load_fence(modifiers: i32) {
    if (modifiers & ACC_VOLATILE) != 0 {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    }
}

/// WP2.1-field — volatile-aware store barrier.
///
/// Mirrors `Unsafe.putReferenceVolatile` / `putIntVolatile` etc.:
/// emit a `Release` fence before the store and a `SeqCst` full fence
/// after. Non-volatile fields skip both to keep plain writes cheap.
fn volatile_store_fence_pre(modifiers: i32) {
    if (modifiers & ACC_VOLATILE) != 0 {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    }
}

fn volatile_store_fence_post(modifiers: i32) {
    if (modifiers & ACC_VOLATILE) != 0 {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// Check if a reflective member access is allowed.
/// `accessible` is true when `setAccessible(true)` has been called on the
/// AccessibleObject (stored in field 7 for Method, field 6 for Field,
/// field 5 for Constructor).
/// Returns Ok(()) if access is allowed, Err(IllegalAccessException) otherwise.
fn check_access(modifiers: i32, accessible: bool, member_desc: &str) -> Result<(), rustjvm_types::error::MethodCallFailed> {
    if accessible || (modifiers & ACC_PUBLIC) != 0 {
        return Ok(());
    }
    Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
        message: format!(
            "cannot access member: modifiers 0x{:04x}, {}",
            modifiers, member_desc,
        ),
    }
    .into())
}

// ---------------------------------------------------------------------------
// NEW-19: JPMS `opens` / `exports` enforcement for reflection (JEP 403)
// ---------------------------------------------------------------------------
//
// These helpers resolve the caller class by walking the Java call stack,
// skipping the `java.lang.reflect.*` and `jdk.internal.reflect.*` frames
// that live between the user's call site and the reflection native. Once
// resolved, the caller's ClassId is combined with the target member's
// declaring class to query `NativeContext::check_deep_reflection_access`.
//
// The check is invoked in two places:
//   1. `AccessibleObject.setAccessible(true)` — where JEP 403 specifies that
//      deep reflection is gated (throws InaccessibleObjectException).
//   2. `Method.invoke` / `Field.get|set` / `Constructor.newInstance` when
//      `accessible == false` — so non-public members in non-exported
//      packages of a different module are not silently reachable.

/// Reflection-internal class-name prefixes that must be skipped when
/// determining the "caller" of a reflective operation. Ordering is
/// insignificant; the list is matched as `starts_with` / equality.
const REFLECTION_INTERNAL_CLASSES: &[&str] = &[
    "java/lang/reflect/",
    "java/lang/invoke/",
    "jdk/internal/reflect/",
    "sun/reflect/",
    "java/lang/Class",
    "java/lang/AccessibleObject",
];

/// Walk the current Java call stack and return the ClassId of the first
/// non-reflection frame — i.e. the user code that invoked the reflection
/// native. Returns `None` if the stack contains no such frame (which
/// happens when the VM is bootstrapping or when reflection is called from
/// a pure native context).
///
/// The stack trace comes from `NativeContext::capture_stack_trace(0)` which
/// returns entries from innermost to outermost. We skip every entry whose
/// `class_name` matches a reflection-internal prefix, then resolve the next
/// user frame to a `ClassId` via `class_id_by_name`.
fn resolve_caller_class_id(ctx: &mut dyn NativeContext) -> Option<ClassId> {
    let trace = ctx.capture_stack_trace(0);
    for entry in &trace {
        let name: &str = &entry.class_name;
        let is_internal = REFLECTION_INTERNAL_CLASSES.iter().any(|prefix| {
            if prefix.ends_with('/') {
                name.starts_with(prefix)
            } else {
                name == *prefix
            }
        });
        if is_internal {
            continue;
        }
        if let Some(cid) = ctx.class_id_by_name(name) {
            return Some(cid);
        }
    }
    None
}

/// Perform the JEP 403 deep-reflection check for a reflective access to a
/// member declared in `target_class_name`.
///
/// `accessible_override` indicates whether the caller has already passed
/// `setAccessible(true)` on the AccessibleObject. Per JEP 403, once the
/// override flag is set the subsequent `invoke/get/set` bypasses the deep
/// check — the check has already been paid at `setAccessible` time.
///
/// Returns the `Err` variant that the caller should propagate:
///   * when called from `setAccessible` → caller turns it into
///     `InaccessibleObjectException`
///   * when called from `invoke/get/set` with `accessible_override == false`
///     → caller turns it into `IllegalAccessException`
///
/// Returns `Ok(())` when:
///   * `accessible_override == true` (JEP 403: the check was already paid)
///   * the target class cannot be resolved (defensive: avoid blocking on
///     an internal lookup failure)
///   * there is no user frame (VM bootstrap — nothing to check against)
///   * `NativeContext::check_deep_reflection_access` approves the edge
fn check_reflection_module_access(
    ctx: &mut dyn NativeContext,
    target_class_name: &str,
    accessible_override: bool,
) -> Result<(), String> {
    if accessible_override {
        // Once setAccessible(true) has been granted, subsequent reflective
        // operations trust the override flag (JEP 403 §"API changes").
        return Ok(());
    }
    let target_cid = match ctx.class_id_by_name(target_class_name) {
        Some(cid) => cid,
        None => return Ok(()),
    };
    let accessor_cid = match resolve_caller_class_id(ctx) {
        Some(cid) => cid,
        // No user frame — either VM bootstrap or all frames are reflection
        // internals. Allow; we are not invoked from Java code.
        None => return Ok(()),
    };
    // JDK-internal callers (java.base classes performing reflection on
    // their own private types — e.g. `StackStreamFactory$StackFrameBuffer.fill`
    // constructing `StackFrameInfo` via `Constructor.newInstance`) must not
    // be subject to the unnamed-module check. Our class loader does not
    // always populate `module_name` for JDK inner classes, so the
    // same-module rule (rule 1) misses and the check falls through to
    // "module java.base does not opens java.lang to unnamed module".
    // The accessor's *package*, not its module assignment, is the reliable
    // signal that we are running JDK-internal code; trust it and skip.
    if let Some(name) = ctx.class_name_of_id(accessor_cid) {
        if name.starts_with("java/")
            || name.starts_with("jdk/")
            || name.starts_with("sun/")
            || name.starts_with("com/sun/")
        {
            return Ok(());
        }
    }
    // Second escape hatch — JDK-internal reflection driven from user code.
    //
    // The JDK reflectively constructs its own private types from inside
    // boot-path machinery (e.g. `StackStreamFactory$StackFrameBuffer.fill`
    // calling `Constructor.newInstance` to build `StackFrameInfo` while a
    // user-code `StackWalker.walk(...)` is in progress). Our interpreter
    // lacks a complete view of those nested JDK frames — `capture_stack_trace`
    // sees only the outermost user frame (typically the SB3 launcher) — so
    // the caller-class-based whitelist above misses and the call gets
    // rejected as if user code were attempting deep reflection on
    // encapsulated JDK internals.
    //
    // Use the *target class name* as a fallback signal: if the type being
    // constructed is a JDK-internal type that user code does not normally
    // instantiate directly (java.lang internal stack-walker frames,
    // jdk.internal.* private impls, sun.* private impls), treat the access
    // as JDK-mediated and allow it. This matches HotSpot's
    // `Module.implAddOpensToAllUnnamed` behaviour for boot modules and the
    // self-opening semantics of `java.base` for its own packages.
    if target_class_name.starts_with("jdk/internal/")
        || target_class_name.starts_with("sun/")
        || target_class_name.starts_with("com/sun/")
        || target_class_name.starts_with("java/lang/StackFrame")
        || target_class_name.starts_with("java/lang/ClassFrame")
        || target_class_name.starts_with("java/lang/StackStreamFactory")
        || target_class_name.starts_with("java/lang/Module")
    {
        return Ok(());
    }
    ctx.check_deep_reflection_access(accessor_cid, target_cid)
}

/// Read a declaring-class mirror from an AccessibleObject and enforce the
/// NEW-19 module check. Returns `Err(IllegalAccessException)` on denial.
///
/// `mirror_field_index` is the slot holding the `Class` mirror on the
/// reflection object (field 0 for Field/Method/Constructor).
fn enforce_module_check_from_mirror(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mirror_field_index: usize,
    accessible: bool,
    operation: &str,
) -> Result<(), rustjvm_types::error::MethodCallFailed> {
    let target_class_name = match ctx.get_field(this, mirror_field_index) {
        Value::Object(Some(m)) => mirror_class_name(ctx, m),
        _ => None,
    };
    if let Some(name) = target_class_name {
        if let Err(msg) = check_reflection_module_access(ctx, &name, accessible) {
            return Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
                message: format!("{operation}: {name}: {msg}"),
            }
            .into());
        }
    }
    Ok(())
}

/// Variant of [`enforce_module_check_from_mirror`] that reads the
/// declaring-class mirror from a Field object via `get_field_by_name`,
/// matching the real JDK layout regardless of hierarchy-dependent slot
/// offsets.
fn enforce_module_check_on_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    accessible: bool,
    operation: &str,
) -> Result<(), rustjvm_types::error::MethodCallFailed> {
    let target_class_name = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(m)) => mirror_class_name(ctx, m),
        _ => None,
    };
    if let Some(name) = target_class_name {
        if let Err(msg) = check_reflection_module_access(ctx, &name, accessible) {
            return Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
                message: format!("{operation}: {name}: {msg}"),
            }
            .into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// java.lang.Class natives
// ---------------------------------------------------------------------------

pub(crate) fn native_class_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Class mirror object)
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Use the reverse map (or legacy field-0 fallback) to resolve the ClassId.
    // If mirror_class_id returns None, this is a primitive mirror — read the
    // name directly from field 1 (the `name` slot in both synthetic and real
    // JDK layouts).
    match mirror_class_id(ctx, this) {
        Some(class_id) => {
            let name = ctx
                .class_name_of_id(class_id)
                .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
            let dotted_name = name.replace('/', ".");
            let name_obj = ctx.create_string(&dotted_name);
            Ok(Some(Value::Object(Some(name_obj))))
        }
        None => {
            // Primitive mirror or unknown — read name from field 1.
            // For array-class mirrors (e.g. `[Ljava/lang/String;`) the internal
            // name uses '/' separators; `Class.getName()` must report the
            // dotted form (`[Ljava.lang.String;`) so Spring's
            // `AnnotationAttributes.assertAttributeType` string-compares
            // against the expected component class name (which is dotted).
            // Plain primitive names like "int"/"void" contain no '/', so the
            // replace is a no-op for them.
            if let Some(prim_name) = mirror_class_name(ctx, this) {
                let dotted_name = prim_name.replace('/', ".");
                let name_obj = ctx.create_string(&dotted_name);
                Ok(Some(Value::Object(Some(name_obj))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

pub(crate) fn native_class_get_primitive_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] = String (primitive type name like "int", "boolean", etc.)
    let name_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };

    let name = ctx.read_string(name_obj).unwrap_or_default();
    let mirror = ctx.primitive_class_mirror(&name);
    Ok(Some(Value::Object(Some(mirror))))
}

// ---------------------------------------------------------------------------
// Step 7: Enhanced Class support
// ---------------------------------------------------------------------------

/// Look up the ClassId backing a Class mirror.  Uses the VM's reverse
/// map first (populated by `get_or_create_class_mirror`).  Falls back to
/// reading field 0 as Int(class_id) for legacy/synthetic compatibility.
///
/// Returns `None` for primitive mirrors (not in the reverse map and
/// field 0 is no longer Int(-1) — it's Object(None) in real-JDK mode).
pub(crate) fn mirror_class_id(
    ctx: &dyn NativeContext,
    mirror: rustjvm_types::ObjectRef,
) -> Option<rustjvm_types::ClassId> {
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        return Some(cid);
    }
    if let Value::Int(v) = ctx.get_field(mirror, 0) {
        if v >= 0 {
            return Some(rustjvm_types::ClassId::new(v as u32));
        }
    }
    None
}

/// Helper: read the **internal** class name from a Class mirror (`pkg/Cls`,
/// `[I`, etc.).
///
/// Prefer the VM reverse-map (`class_id_from_mirror` → [`NativeContext::class_name_of_id`])
/// for real `java.lang.Class` instances — JDK 25 may store something other
/// than the internal name at slot 1, which broke `getPackage` and reflective
/// constructor matching for Spring Boot.
///
/// When the mirror is **not** in the reverse map (legacy/unit-test mirrors
/// that encode `ClassId` only as `int` field 0 and put the internal name in
/// slot 1), trust slot 1 first; only if it is missing do we fall back to
/// [`mirror_class_id`] + `class_name_of_id`.
pub(crate) fn mirror_class_name(ctx: &dyn NativeContext, mirror: rustjvm_types::ObjectRef) -> Option<String> {
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        return ctx.class_name_of_id(cid);
    }
    let slot1 = match ctx.get_field(mirror, 1) {
        Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
        _ => None,
    };
    if let Some(ref s) = slot1 {
        if !s.is_empty() {
            return slot1;
        }
    }
    mirror_class_id(ctx, mirror).and_then(|cid| ctx.class_name_of_id(cid))
}

pub(crate) fn native_class_is_record(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_rec = mirror_class_id(ctx, this)
        .map(|cid| ctx.is_record_class(cid))
        .unwrap_or(false);
    Ok(Some(Value::Int(if is_rec { 1 } else { 0 })))
}

pub(crate) fn native_class_is_sealed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_sealed = mirror_class_id(ctx, this)
        .map(|cid| ctx.is_sealed_class(cid))
        .unwrap_or(false);
    Ok(Some(Value::Int(if is_sealed { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// T19_H10_RESOURCE_VALIDATION: shared resource-name guard used by both
// `Class.getResourceAsStream` and `Class.getResource` (and any future
// `getResource[s]` delegator).  Returns `None` on rejection — caller should
// surface that as a null InputStream / null URL, exactly matching what the
// real JDK does for resources that fail policy.
//
// Hardening rules (all per the T19.H10 brief, security section):
//   * Empty / >256 byte name → reject. The 256-byte cap is conservative;
//     `keycloak-version.properties` is 28 bytes, the longest legitimate JDK
//     resource we've observed (`META-INF/services/java.security.Provider`)
//     is 49 bytes, and the JLS does not bound the name so we pick a length
//     that is comfortably above legitimate usage but small enough to keep
//     a malicious caller from forcing a large allocation in `find_resource`.
//   * Any byte < 0x20 or == 0x7F (DEL) → reject. NUL injects through native
//     POSIX paths; the rest are control bytes that have no business in a
//     resource name and tend to indicate a corrupted constant-pool entry.
//   * `..` segment → reject. Even though `find_resource` re-canonicalizes,
//     defence-in-depth keeps a malicious resource name from sneaking past
//     a future change to the class-path matcher.
//   * Backslash → reject. Resource paths are forward-slash on every JVM
//     platform; backslash here is a Windows-path-injection signal.
//
// The name passed in already has its leading `/` stripped (the package-
// prefix prepending happens before this guard), so we do not need to re-
// trim. `ResolvedResource` keeps `is_absolute` so the caller can decide
// whether to mix in the package prefix on a relative name.
// ---------------------------------------------------------------------------

/// Public-crate alias of [`t19_h10_validate_resource_name`] so the
/// ClassLoader-side getResourceAsStream native can share the exact same
/// policy without re-implementing it (and silently drifting from the
/// Class-side path).  Returns `Some(name)` on accept, `None` on reject.
pub(crate) fn t19_h10_validate_resource_name_pub(name: &str) -> Option<&str> {
    t19_h10_validate_resource_name(name)
}

/// Validate a resolved (post-stripping, post-package-prefix) resource name.
/// Returns `Some(name)` if the name passes every policy check above, or
/// `None` if the caller should fail with a null result.
fn t19_h10_validate_resource_name(name: &str) -> Option<&str> {
    if name.is_empty() || name.len() > 256 {
        return None;
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return None;
    }
    if name.contains('\\') {
        return None;
    }
    // Reject `..` only as a path segment, not as a substring of a real
    // filename like `foo..bar.txt`.
    for seg in name.split('/') {
        if seg == ".." {
            return None;
        }
    }
    Some(name)
}

/// Resolve a `Class.getResource[AsStream]` argument to the canonical
/// classpath-relative key. Returns `None` for any name that fails the
/// T19_H10 validation policy or has a null `name` argument upstream.
///
/// The two callers (`getResourceAsStream`, `getResource`) used to inline
/// this logic separately, which made the package-prefix and validation
/// rules drift out of sync. Centralising it keeps both natives honest.
fn t19_h10_resolve_resource_name(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Option<String> {
    let resolved = if let Some(stripped) = name.strip_prefix('/') {
        stripped.to_string()
    } else {
        let class_name = mirror_class_name(ctx, this).unwrap_or_default();
        match class_name.rsplit_once('/') {
            Some((pkg, _)) => format!("{pkg}/{name}"),
            None => name.to_string(),
        }
    };
    t19_h10_validate_resource_name(&resolved).map(|s| s.to_string())
}

/// Allocate a real-JDK-compatible `java.io.ByteArrayInputStream` over the
/// supplied byte slice and return its `ObjectRef`. Used by both the
/// `Class.getResourceAsStream` path and the `ClassLoader.getResourceAsStream`
/// path so the two stay layout-identical (`buf=0, pos=1, mark=2, count=3`).
///
/// The dual-write (slot index + by-name) handles the case where the class
/// hasn't been loaded yet — `set_field_by_name` is a no-op pre-load — while
/// staying correct for a real-JDK BAIS once loaded.
pub(crate) fn t19_h10_alloc_byte_array_input_stream(
    ctx: &mut dyn NativeContext,
    bytes: &[u8],
) -> ObjectRef {
    let len = bytes.len();
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, len);
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(len as i32));
    ctx.set_field_by_name(stream, "buf", Value::Object(Some(arr)));
    ctx.set_field_by_name(stream, "pos", Value::Int(0));
    ctx.set_field_by_name(stream, "mark", Value::Int(0));
    ctx.set_field_by_name(stream, "count", Value::Int(len as i32));
    stream
}

/// T14/T15 + T19.H10: `Class.getResourceAsStream(String)` — bypasses the
/// Module-based resolution path in real JDK 25 bytecode and delegates
/// directly to the resource lookup, returning a `ByteArrayInputStream`
/// over the resource bytes.
///
/// T19.H10 hardening:
///   * Resource-name validation: empty / oversized / control-byte / `..` /
///     backslash names are rejected before reaching `find_resource`,
///     preventing a malicious classpath caller from coercing the lookup
///     into a path-traversal probe or a large-allocation request.
///   * Null `name` argument now returns null (matches JDK 25 spec —
///     `Class.getResourceAsStream(null)` throws NPE in the JDK, but the
///     callers that actually hit this native always pass a non-null
///     constant-pool string; the bytecode-level NPE is the right answer
///     and the Class-mirror path is unchanged from prior behaviour).
///
/// Called with args[0] = this (Class mirror), args[1] = name (String).
pub(crate) fn native_class_get_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let resource_name = match t19_h10_resolve_resource_name(ctx, this, &name) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    match ctx.find_resource(&resource_name) {
        None => Ok(Some(Value::Object(None))),
        Some(bytes) => {
            let len = bytes.len();
            let stream = t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
            tracing::debug!(
                target: "rustjvm_vm::runtime::resources",
                resource = %resource_name,
                bytes = len,
                "Class.getResourceAsStream served resource"
            );
            Ok(Some(Value::Object(Some(stream))))
        }
    }
}

/// T14/T15 + T19.H10: `Class.getResource(String)` — returns a URL pointing
/// at the resource, or null if absent. Now synthesises a real
/// `java.net.URL` whose `protocol="resource"`, `host=""`, `port=-1`, and
/// `file = "/<resolved-name>"`. Real-JDK code that does
/// `getResource(...).toString()` or `.openStream()` therefore sees a stable
/// non-null URL whose external form round-trips through `new URL(s)`.
///
/// The resource scheme is bespoke; we do not pretend to be `jrt:`,
/// `file:`, or `jar:` because (a) we don't have a URLStreamHandler
/// registry that handles those schemes natively yet, and (b) downstream
/// code that key-on-URL semantics (Resource leaks tracker, ProtectionDomain
/// caching) is happier with a stable identity than with a synthesised
/// `file:` URL that points to nowhere on disk. Callers that need the bytes
/// should round-trip through `getResourceAsStream`, which this native
/// shares the validation helper with — keeping the two halves consistent.
pub(crate) fn native_class_get_resource(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let resource_name = match t19_h10_resolve_resource_name(ctx, this, &name) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    match ctx.find_resource(&resource_name) {
        None => Ok(Some(Value::Object(None))),
        Some(_bytes) => {
            // Synthesise a `java.net.URL` with scheme=`resource`, file=`/<name>`.
            // Field layout matches the real JDK: protocol/host/file/authority/
            // ref/userInfo as String, port as int. Slot indices are taken
            // from the `essential-natives`-loaded URL stub; for real-JDK
            // mode we additionally write by name so any layout drift is
            // forgiving.
            let url = alloc_concurrent_synthetic(ctx, "java/net/URL", 8);
            let proto_s = ctx.create_string("resource");
            let host_s = ctx.create_string("");
            let file_s = ctx.create_string(&format!("/{}", resource_name));
            ctx.set_field_by_name(url, "protocol", Value::Object(Some(proto_s)));
            ctx.set_field_by_name(url, "host", Value::Object(Some(host_s)));
            ctx.set_field_by_name(url, "port", Value::Int(-1));
            ctx.set_field_by_name(url, "file", Value::Object(Some(file_s)));
            ctx.set_field_by_name(url, "path", Value::Object(Some(file_s)));
            Ok(Some(Value::Object(Some(url))))
        }
    }
}

/// RKC16r23 — detect jboss-logging's i18n localized-logger fallback names.
///
/// jboss-logging generates classes named `<base>_$logger` and walks the
/// `Class.forName` chain `_$logger_<lang>_<country>` → `_$logger_<lang>` →
/// `_$logger` (then a parallel `_$bundle` chain). Only the base names ship
/// in app jars; the locale-suffixed variants always CNFE.
///
/// Returns true if `name` ends with `_$logger_<token>` or
/// `_$logger_<token>_<token>` (or the `_$bundle_` equivalents) where
/// every locale token is two-or-three ASCII lowercase letters
/// (`<lang>`) or two ASCII uppercase letters (`<country>`).
fn i18n_logger_locale_suffix(name: &str) -> bool {
    let marker_logger = "_$logger_";
    let marker_bundle = "_$bundle_";
    let suffix = if let Some(idx) = name.rfind(marker_logger) {
        &name[idx + marker_logger.len()..]
    } else if let Some(idx) = name.rfind(marker_bundle) {
        &name[idx + marker_bundle.len()..]
    } else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    let mut parts = suffix.split('_');
    let lang = parts.next().unwrap_or("");
    let is_lang = (2..=3).contains(&lang.len())
        && lang.chars().all(|c| c.is_ascii_lowercase());
    if !is_lang {
        return false;
    }
    match parts.next() {
        None => true, // `_$logger_<lang>`
        Some(country) => {
            // `_$logger_<lang>_<country>` (no further parts; variant possible
            // but rare — accept country=2 upper or 2-3 alphanum).
            let ok = country.len() == 2
                && country.chars().all(|c| c.is_ascii_uppercase());
            ok && parts.next().is_none()
        }
    }
}

/// RKC16r23 — gate the verbose `Class.forName` diagnostic prints behind an
/// env var so they don't pollute boot logs in normal runs. Set
/// `RUSTJVM_S111_DBG=1` to re-enable.
fn s111_dbg_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RUSTJVM_S111_DBG").is_ok())
}

macro_rules! s111_dbg {
    ($($arg:tt)*) => {
        if crate::lang_class::s111_dbg_enabled() {
            eprintln!($($arg)*);
        }
    };
}

pub(crate) fn native_class_for_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.forName: name is null".to_string()),
            }
            .into())
        }
    };
    let dotted_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal_name = dotted_name.replace('.', "/");

    // RKC16r23 — jboss-logging i18n localized-logger lookup short-circuit.
    //
    // jboss-logging's `Messages.getBundle` / `LoggerProviders.doGetMessageLogger`
    // walks a fallback chain of `<FQCN>_$logger_<lang>_<country>`,
    // `<FQCN>_$logger_<lang>`, `<FQCN>_$logger` for every i18n logger lookup.
    // The first two almost never exist (no app ships per-locale generated
    // classes); they're spec'd to throw CNFE so the chain tries the next.
    //
    // Inside CratonVM, each `loadClass` round-trip for a missing class is
    // expensive: ModuleClassLoader's `loadClass` walks the dependency graph,
    // tries every resource-root JAR, fails, throws CNFE. For Keycloak this
    // happens on every WARN/ERROR log call — turning what should be a no-op
    // cached negative into a tight loop of class-loader walks that visibly
    // dominates boot time.
    //
    // Real OpenJDK has the same CNFE cost in principle but jboss-logging's
    // own `Messages` infrastructure caches the resolved bundle Class per
    // `Class<?>` key, so the chain only walks once per bundle interface. In
    // our run the cache lookup isn't hitting (likely the `Messages` static
    // field is being re-clinit'd or the WeakReference clears) — and rather
    // than reverse-engineer jboss-logging's internals, we short-circuit the
    // negative answer at the VM boundary: any name matching the
    // `<...>_$logger_<lang>(_<country>)?` suffix where the suffix's
    // language and (optional) country tokens look like locale codes
    // returns CNFE immediately without calling out to loader.loadClass.
    if i18n_logger_locale_suffix(&dotted_name) {
        return Err(rustjvm_types::error::RuntimeError::ClassNotFoundException {
            class_name: dotted_name,
        }
        .into());
    }

    // RKC16N.12 — when `Class.forName` is invoked with an explicit non-null
    // classloader, route through `loader.loadClass(name)` so module-scoped
    // loaders (notably `org.jboss.modules.ModuleClassLoader`) get their
    // visibility-closure search. Without this, JBoss Modules' boot path
    // (`Module.run` → `Class.forName(mainClass, false, mcl)` at PC 40 of
    // `Module.run(String,String[])`) bypasses the module's resource-roots
    // and the bootstrap classpath has no entry for module-private classes
    // like `org.jboss.as.server.Main` — so KC16 boot dies with CNFE before
    // reaching `Main.main`.
    //
    // Argument layout per JDK 25 `Class.forName0(String,boolean,ClassLoader,Class)`:
    //   args[0] = name (String, already read above)
    //   args[1] = initialize (boolean)
    //   args[2] = loader (ClassLoader, may be null = bootstrap)
    //   args[3] = caller (Class, ignored by us)
    if let Some(Value::Object(Some(loader))) = args.get(2) {
        let loader_class_name_debug = {
            let cid = ctx.class_id_of_object(*loader);
            ctx.class_name_of_id(cid).unwrap_or_default()
        };
        s111_dbg!("[S111-DBG] Class.forName({}) loader={}", dotted_name, loader_class_name_debug);
        let invoke_args = [Value::Object(Some(*loader)), Value::Object(Some(name_obj))];
        match ctx.invoke_virtual(
            *loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &invoke_args[1..],
        ) {
            Ok(Some(mirror)) => {
                s111_dbg!("[S111-DBG] loadClass({}) succeeded via invoke_virtual", dotted_name);
                return Ok(Some(mirror));
            }
            // ClassLoader.loadClass returning null is technically illegal
            // (per spec it must throw CNFE) but defensively translate it.
            Ok(None) => {
                s111_dbg!("[S111-DBG] loadClass({}) returned null", dotted_name);
                return Err(rustjvm_types::error::RuntimeError::ClassNotFoundException {
                    class_name: dotted_name,
                }
                .into())
            }
            // S111r12 — NSME on `loader.loadClass` rescue. Spring Boot 2's
            // SB2 launcher path delivers a `LaunchedURLClassLoader`
            // instance whose `class_id_of` returns `java/lang/Comparable`
            // (the loader inherited that stub class_id somewhere in the
            // boot chain). The receiver-driven virtual dispatch then
            // resolves to `Comparable.loadClass` and raises NSME.
            // Fall back to bootstrap-style class loading so the
            // `Class.forName(name, init, loader)` chain still resolves.
            Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Linkage(
                    rustjvm_types::error::LinkageError::NoSuchMethodError { .. },
                ),
            )) => {
                s111_dbg!("[S111-DBG] loadClass({}) -> NoSuchMethodError, fallback", dotted_name);
                // Fall through to bootstrap-style ensure_class_initialized below.
            }
            // S111r20 — Spring Boot 2.x LaunchedURLClassLoader.loadClass
            // fails in CratonVM because the JDK bytecode for URLClassPath
            // walks nested-JAR URLs (jar:file:/fat.jar!/BOOT-INF/lib/foo.jar!/)
            // via JarURLConnection + the Spring Boot custom jar: Handler. These
            // lower-level primitives are not wired up in CratonVM's real-JDK
            // mode, so loadClass throws ClassNotFoundException (Java-level) or
            // fails internally. In both cases, fall through to bootstrap-style
            // ensure_class_initialized, which uses CratonVM's built-in classpath
            // scanner (which already extracted the nested JARs from BOOT-INF/lib).
            //
            // We do NOT apply this rescue to module-aware class loaders like
            // org.jboss.modules.ModuleClassLoader — for those, CNFE is the
            // authoritative answer about module visibility. Detect Spring Boot's
            // LaunchedURLClassLoader by class name prefix.
            Err(rustjvm_types::error::MethodCallFailed::ExceptionThrown(_)) => {
                // Check if this is a LaunchedURLClassLoader (Spring Boot 2/3)
                // or a URLClassLoader subclass that might have the same issues.
                // For these, bootstrap fallback is safe. For module loaders, propagate.
                let loader_class_name = loader_class_name_debug.clone();
                let is_launched_url_cl = loader_class_name.contains("LaunchedURLClassLoader")
                    || loader_class_name.contains("launch/LaunchedURLClassLoader")
                    || loader_class_name == "java/net/URLClassLoader";
                if is_launched_url_cl {
                    s111_dbg!("[S111-DBG] loadClass({}) -> ExceptionThrown for LaunchedURLCL, fallback", dotted_name);
                    // Fall through to ensure_class_initialized below.
                } else {
                    // For module-scoped loaders (JBoss Modules, OSGi, etc.)
                    // the CNFE is authoritative — propagate it.
                    return Err(rustjvm_types::error::RuntimeError::ClassNotFoundException {
                        class_name: dotted_name,
                    }.into());
                }
            }
            // Propagate internal VM errors without re-wrapping.
            Err(e) => {
                s111_dbg!("[S111-DBG] loadClass({}) -> InternalError {:?}, propagating", dotted_name, e);
                return Err(e);
            }
        }
        s111_dbg!("[S111-DBG] falling through to ensure_class_initialized({})", dotted_name);
    }

    match ctx.ensure_class_initialized(&internal_name) {
        Ok(class_id) => {
            // WP2.10 — JDK 25 spec: hidden classes (created via
            // Lookup.defineHiddenClass) are NOT discoverable by name.
            // `Class.forName` must throw ClassNotFoundException for them
            // even though they are loaded.
            if ctx.is_class_hidden(class_id) {
                return Err(rustjvm_types::error::RuntimeError::ClassNotFoundException {
                    class_name: dotted_name,
                }
                .into());
            }
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(e) => {
            if let rustjvm_types::error::MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                let exc_cid = ctx.class_id_of_object(*exc_ref);
                let exc_class = ctx.class_name_of_id(exc_cid).unwrap_or_default();
                let msg = match ctx.get_field(*exc_ref, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                s111_dbg!("[FORNAME-ERR] name={} exc_class={} msg={}", dotted_name, exc_class, msg);
            } else {
                s111_dbg!("[FORNAME-ERR] name={} err={:?}", dotted_name, e);
            }
            Err(rustjvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: dotted_name,
            }
            .into())
        },
    }
}

pub(crate) fn native_class_for_name_3(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Same as forName(String) but ignores initialize flag and classLoader
    native_class_for_name(ctx, args)
}

// T19_H12_FORNAME_MODULE — `Class.forName(Module, String)` per JDK 25 spec.
//
// JDK's stock bytecode for this overload resolves `module.getClassLoader()`
// then invokes `cl.loadClass(module, name)`. Our synthetic `java.lang.Module`
// objects (built by `phases_late.rs::ModuleLayer.modules` and
// `jboss_jdkspecific.rs::build_module`) have a 5-slot layout where slot 2
// is `packages: Set<String>` (a `HashSet` populated with JDK package names).
// On a real JDK 25 `java.lang.Module` class file the field at the
// equivalent offset is `loader: ClassLoader`. The mismatch caused JDK
// bytecode to dispatch `loadClass(Module, String)` against the HashSet
// receiver and surface a `NoSuchMethodError: HashSet.loadClass(...)`.
//
// Bypassing the JDK bytecode and resolving directly through the bootstrap
// loader is correct: every Module our VM hands out for platform classes
// (java.base, java.logging, java.management, ...) is in the boot layer
// and would resolve via the bootstrap `BootLoader.loadClass(module, name)`
// branch in real JDK anyway.
//
// Per JEP 261 / Class.forName(Module, String) javadoc:
//   - If `module` is null → NullPointerException
//   - If `name` is null → NullPointerException
//   - If the class is not found in the module → return null (NOT throw CNFE)
//   - Hidden classes are not discoverable
pub(crate) fn native_class_for_name_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Module, args[1] = String name
    match args.first() {
        Some(Value::Object(Some(_))) => {}
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.forName: module is null".to_string()),
            }
            .into());
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.forName: name is null".to_string()),
            }
            .into());
        }
    };
    let dotted_name = ctx.read_string(name_obj).unwrap_or_default();
    if dotted_name.is_empty() {
        // JDK rejects empty names by returning null per Class.forName(Module, String) spec.
        return Ok(Some(Value::Object(None)));
    }
    // Hardening: reject names that look like a class-load injection.
    // Names with NUL, control bytes, or path separators are never valid
    // class names and would let an attacker probe the filesystem.
    if dotted_name.bytes().any(|b| b < 0x20 || b == 0x7F)
        || dotted_name.contains('/')
        || dotted_name.contains('\\')
    {
        return Ok(Some(Value::Object(None)));
    }
    let internal_name = dotted_name.replace('.', "/");
    match ctx.ensure_class_initialized(&internal_name) {
        Ok(class_id) => {
            // Hidden classes are not discoverable via Class.forName per JEP 371.
            if ctx.is_class_hidden(class_id) {
                return Ok(Some(Value::Object(None)));
            }
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        // Spec: this overload returns null (NOT throws CNFE) on miss.
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_class_is_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        Some(Value::Object(None)) => return Ok(Some(Value::Int(0))),
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let target_class_id = ctx.class_id_of_object(target);

    // S111r17 — Array-aware isInstance.  Heap-stored `class_id_of` for an
    // array returns the COMPONENT class id (e.g. `java/lang/Class` for a
    // `Class[]` array), NOT the synthetic `[Lcomponent;` array class id.
    // Without this branch, `Class[].isInstance(myClassArray)` reduces to
    // `is_subclass(java/lang/Class, [Ljava/lang/Class;)` which is always
    // false — the same bug that the interpreter's `instanceof` bytecode
    // already works around in `array_descriptor_of` /
    // `array_is_assignable_to`.  Mirror that logic here so reflection
    // callers (Spring's `TypeMappedAnnotation.adapt`, which throws
    // `IllegalArgumentException` when `type.isInstance(value)` returns
    // false for a wrapped attribute array) see consistent results.
    let target_is_array = ctx.heap_kind_of(target) == rustjvm_types::ObjectKind::Array;
    if target_is_array {
        let src_desc = array_descriptor_for(ctx, target);
        let this_name = mirror_class_name(ctx, this).unwrap_or_default();
        if array_is_assignable(ctx, &src_desc, &this_name) {
            return Ok(Some(Value::Int(1)));
        }
        // Fall through to legacy id-based check below (covers some
        // edge cases where the target is an array but the type mirror
        // is a non-array Class — those reduce to assignable-to-Object
        // via `array_is_assignable` already, so this is just defensive).
    }

    // Annotation proxy special case: our `create_annotation_proxy` allocates
    // objects of class `java/lang/annotation/AnnotationProxy`, not of the
    // actual annotation interface. JDK reflection and Spring's
    // `AttributeMethods.assertAnnotation` do `annotationType.isInstance(ann)`
    // which would otherwise return false (proxy class doesn't extend / implement
    // the user-declared annotation interface), driving Spring into
    // `Assert.instanceCheckFailed` and an unrelated NPE during message
    // formatting. Read `ANN_PROXY_TYPE_MIRROR` (slot 1) and treat the proxy
    // as an instance of the recorded annotation type (and any of its
    // super-interfaces, including `java.lang.annotation.Annotation`).
    if let Some(target_name) = ctx.class_name_of_id(target_class_id) {
        if target_name == "java/lang/annotation/AnnotationProxy" {
            if let Value::Object(Some(type_mirror)) = ctx.get_field(target, ANN_PROXY_TYPE_MIRROR) {
                if let Some(ann_cid) = mirror_class_id(ctx, type_mirror) {
                    let proxy_matches = ann_cid == this_class_id
                        || ctx.is_subclass(ann_cid, this_class_id);
                    if proxy_matches {
                        return Ok(Some(Value::Int(1)));
                    }
                    // Always treat proxies as instances of `Annotation`
                    // itself, even if the type-mirror class graph doesn't
                    // record the implements-edge yet.
                    if let Some(this_name) = ctx.class_name_of_id(this_class_id) {
                        if this_name == "java/lang/annotation/Annotation" {
                            return Ok(Some(Value::Int(1)));
                        }
                    }
                }
            }
        }
    }

    let result =
        target_class_id == this_class_id || ctx.is_subclass(target_class_id, this_class_id);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// S111r17 — Build the JVMS array descriptor (e.g. `[Ljava/lang/Class;`,
/// `[I`, `[[Ljava/lang/String;`) for an array heap object.  Mirrors the
/// interpreter's `array_descriptor_of` (vm/src/runtime/interpreter.rs)
/// but lives in NativeContext-land so reflection natives can use it.
fn array_descriptor_for(ctx: &dyn NativeContext, obj: rustjvm_types::ObjectRef) -> String {
    use rustjvm_types::ArrayElementType;
    let et = ctx.heap_element_type_of(obj);
    match et {
        ArrayElementType::Boolean => "[Z".to_string(),
        ArrayElementType::Char => "[C".to_string(),
        ArrayElementType::Float => "[F".to_string(),
        ArrayElementType::Double => "[D".to_string(),
        ArrayElementType::Byte => "[B".to_string(),
        ArrayElementType::Short => "[S".to_string(),
        ArrayElementType::Int => "[I".to_string(),
        ArrayElementType::Long => "[J".to_string(),
        ArrayElementType::Reference => {
            let comp_id = ctx.class_id_of_object(obj);
            let comp_name = ctx.class_name_of_id(comp_id).unwrap_or_default();
            if comp_name.is_empty() {
                "[Ljava/lang/Object;".to_string()
            } else if comp_name.starts_with('[') {
                format!("[{}", comp_name)
            } else {
                format!("[L{};", comp_name)
            }
        }
    }
}

/// S111r17 — Recursive array assignability check, mirroring the
/// interpreter's `array_is_assignable_to`.  `target_name` is a
/// "raw" class name (slash-separated, no `L...;` wrapping for
/// non-array types) — matches what `mirror_class_name` returns.
fn array_is_assignable(ctx: &dyn NativeContext, src_desc: &str, target_name: &str) -> bool {
    if target_name == "java/lang/Object"
        || target_name == "java/io/Serializable"
        || target_name == "java/lang/Cloneable"
    {
        return true;
    }
    if !target_name.starts_with('[') {
        return false;
    }
    if src_desc == target_name {
        return true;
    }
    let src_rest = &src_desc[1..];
    let tgt_rest = &target_name[1..];
    if src_rest.len() == 1 && "ZCBSIJFD".contains(&src_rest[..1]) {
        return src_rest == tgt_rest;
    }
    if tgt_rest.len() == 1 && "ZCBSIJFD".contains(&tgt_rest[..1]) {
        return false;
    }
    let extract = |desc: &str| -> Option<(bool, String)> {
        if desc.starts_with('[') {
            Some((true, desc.to_string()))
        } else if desc.starts_with('L') && desc.ends_with(';') {
            Some((false, desc[1..desc.len() - 1].to_string()))
        } else {
            None
        }
    };
    let (src_is_arr, src_comp) = match extract(src_rest) {
        Some(x) => x,
        None => return false,
    };
    let (tgt_is_arr, tgt_comp) = match extract(tgt_rest) {
        Some(x) => x,
        None => return false,
    };
    if src_is_arr && tgt_is_arr {
        return array_is_assignable(ctx, &src_comp, &tgt_comp);
    }
    if src_is_arr != tgt_is_arr {
        if !src_is_arr {
            return false;
        }
        return tgt_comp == "java/lang/Object"
            || tgt_comp == "java/io/Serializable"
            || tgt_comp == "java/lang/Cloneable";
    }
    if src_comp == "java/lang/Object" {
        return true;
    }
    let src_id = match ctx.class_id_by_name(&src_comp) {
        Some(id) => id,
        None => return false,
    };
    let tgt_id = match ctx.class_id_by_name(&tgt_comp) {
        Some(id) => id,
        None => return false,
    };
    src_id == tgt_id || ctx.is_subclass(src_id, tgt_id)
}

pub(crate) fn native_class_is_assignable_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.isAssignableFrom: argument is null".to_string()),
            }
            .into())
        }
    };

    // S111r17 — Array-aware isAssignableFrom.  Same root cause as
    // `native_class_is_instance` above: when `this` represents an array
    // Class (descriptor like `[Lfoo;` recoverable from the mirror's
    // name field) and `other` represents an array Class, the simple
    // `is_subclass` walk doesn't traverse JVM-level array covariance
    // (e.g. `String[] -> Object[]`, `AnnotationProxy[] ->
    // Annotation[]`).  Reuse the same descriptor / assignability
    // helpers we added for `isInstance`.  Spring 5.x's
    // `Assert.isAssignable(supertype, subtype)` is the visible caller
    // — it throws `IllegalArgumentException` from
    // `assignableCheckFailed` when this returns false, masking the
    // underlying type-system gap.
    let this_name = mirror_class_name(ctx, this).unwrap_or_default();
    let other_name = mirror_class_name(ctx, other).unwrap_or_default();
    if this_name.starts_with('[') || other_name.starts_with('[') {
        // Build descriptors. Non-array Class mirrors get an `L...;`
        // wrap to match the array_is_assignable contract; array
        // mirrors keep their leading `[`.
        let to_desc = |n: &str| -> String {
            if n.starts_with('[') {
                n.to_string()
            } else if n.is_empty() {
                "Ljava/lang/Object;".to_string()
            } else {
                format!("L{};", n)
            }
        };
        let other_desc = to_desc(&other_name);
        let this_desc = to_desc(&this_name);
        // For arrays, dispatch to array_is_assignable (which expects
        // a target that may be an array name, raw class name, or
        // Object/Serializable/Cloneable). Pass other's full descriptor
        // as src and this's name (NOT descriptor) as target.
        if other_desc.starts_with('[') && array_is_assignable(ctx, &other_desc, &this_name) {
            return Ok(Some(Value::Int(1)));
        }
        if this_desc.starts_with('[') && other_desc == this_desc {
            return Ok(Some(Value::Int(1)));
        }
        // Fall through to id-based check below for non-array vs array
        // mismatches (e.g. `String.class.isAssignableFrom(stringArray.class)`
        // → false, handled by the legacy is_subclass which always
        // returns false for these).
    }

    let this_class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let other_class_id = match mirror_class_id(ctx, other) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let result = other_class_id == this_class_id || ctx.is_subclass(other_class_id, this_class_id);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_class_is_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let result = name.starts_with('[');
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_class_is_interface(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let result = ctx.is_interface_class(class_id);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_class_is_primitive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // WP4.2: real-JDK Class layout stores `primitive:boolean` at instance
    // slot 7 — `get_or_create_primitive_mirror` writes Int(1) there for
    // primitive mirrors. Prefer that over the name-based check, because
    // the name slot can be observed as a stale Int after early-boot
    // descriptor-aware coercion runs (the legacy Int(-1) sentinel that
    // `prim_mirror_create` writes into slot 0 leaks into slot 1 read-back
    // under some boot orderings, leaving `mirror_class_name` returning
    // None and primitive `Class.getName()` returning null).
    if let Value::Int(flag) = ctx.get_field(this, 7) {
        if flag != 0 {
            return Ok(Some(Value::Int(1)));
        }
    }
    // Fallback: the legacy synthetic-mode layout where the only signal
    // for primitive-ness is the name string.
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let result = matches!(
        name.as_str(),
        "int" | "long" | "float" | "double" | "boolean" | "char" | "byte" | "short" | "void"
    );
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_class_get_superclass(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    // Per JLS 8.1.4 / `Class.getSuperclass()` spec: returns `null` if this
    // Class represents an interface, the Object class, a primitive type,
    // or void.
    //
    // G2-fix (NegativeArraySizeException): the class file of an interface
    // stores `super_class = java/lang/Object`, so `superclass_of` returns
    // `Some(Object)`. Without this gate, `Class.privateGetPublicMethods()`
    // (the real JDK Java method that backs `Class.getMethods()`) walks
    // into Object's public methods for an interface and returns 14
    // entries (Object's `toString`/`hashCode`/`getClass`/`notify`/
    // `notifyAll`/`wait`/etc. — most 0-arg). ByteBuddy's
    // `JavaDispatcher$DynamicClassLoader.invoker()` then computes
    // `parameterTypes.length - 1` and `anewarray Type[-1]`, raising
    // `NegativeArraySizeException` from `JavaDispatcher.<clinit>`.
    //
    // Real JDK 25 returns `null` here for interfaces — we must match.
    if ctx.is_interface_class(class_id) {
        return Ok(Some(Value::Object(None)));
    }
    match ctx.superclass_of(class_id) {
        Some(parent_id) => {
            let mirror = ctx.get_class_mirror(parent_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_class_get_simple_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    // Strip everything up to the last '/' or '.'
    let simple = name.rsplit(&['/', '.'][..]).next().unwrap_or(&name);
    // Also strip inner class prefix ($)
    let simple = simple.rsplit('$').next().unwrap_or(simple);
    let result = ctx.create_string(simple);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_class_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.newInstance on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NotImplemented {
                feature: "Class.newInstance: no class_id".to_string(),
            }
            .into())
        }
    };
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "unknown".to_string());

    // Allocate and call <init>()V
    let obj_val = ctx.new_object(&class_name)?;
    let obj = match obj_val {
        Some(Value::Object(Some(obj))) => obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NotImplemented {
                feature: "Class.newInstance: new_object failed".to_string(),
            }
            .into())
        }
    };

    // Call <init>()V
    ctx.invoke(&class_name, "<init>", "()V", &[Value::Object(Some(obj))])?;
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// Test harness: tempPrint
// ---------------------------------------------------------------------------
// Reflection helpers
// ---------------------------------------------------------------------------

/// Convert a JVM type descriptor to a Class mirror object.
///
/// Handles primitives ("I" → int.class), object types ("Ljava/lang/String;" → String.class),
/// array types ("[I" → int[].class), and void ("V" → void.class).
pub(crate) fn descriptor_to_class_mirror(ctx: &mut dyn NativeContext, desc: &str) -> rustjvm_types::ObjectRef {
    match desc {
        "I" => ctx.primitive_class_mirror("int"),
        "Z" => ctx.primitive_class_mirror("boolean"),
        "B" => ctx.primitive_class_mirror("byte"),
        "C" => ctx.primitive_class_mirror("char"),
        "S" => ctx.primitive_class_mirror("short"),
        "J" => ctx.primitive_class_mirror("long"),
        "F" => ctx.primitive_class_mirror("float"),
        "D" => ctx.primitive_class_mirror("double"),
        "V" => ctx.primitive_class_mirror("void"),
        s if s.starts_with('L') && s.ends_with(';') => {
            let class_name = &s[1..s.len() - 1];
            match ctx.ensure_class_initialized(class_name) {
                Ok(class_id) => ctx.get_class_mirror(class_id),
                Err(_) => synthetic_class_mirror(ctx, class_name),
            }
        }
        s if s.starts_with('[') => {
            // WP2.2-X: Array type — return the *canonical* Class mirror
            // (same one ldc/anewarray/etc. produce). This is required for
            // reference identity used by JDK Class.java's `arrayContentsEq`
            // in `searchMethods` (compares `Class<?>` references with `!=`).
            // Without this, a Method whose param type is `int[]` will not
            // match the user's `int[].class` and `getDeclaredMethod`
            // throws NSME. Falls back to a synthetic mirror only if the
            // class manager refuses to register the array class.
            match ctx.ensure_class_initialized(s) {
                Ok(class_id) => ctx.get_class_mirror(class_id),
                Err(_) => synthetic_class_mirror(ctx, s),
            }
        }
        _ => {
            // Unknown descriptor — return a synthetic mirror.
            synthetic_class_mirror(ctx, desc)
        }
    }
}

/// Create a synthetic java/lang/Class mirror carrying `name` in slot 1.
///
/// C37: the mirror MUST be allocated with `class_id = java/lang/Class` (not
/// `ClassId(0)` / java.lang.Object).  Otherwise `invokevirtual Class.isArray`
/// on the returned mirror walks up Object's superclass chain and raises
/// `NoSuchMethodError: java/lang/Object.isArray()Z`.
fn synthetic_class_mirror(ctx: &mut dyn NativeContext, name: &str) -> rustjvm_types::ObjectRef {
    // Resolve java/lang/Class — ensure it's loaded so we get its real ClassId.
    let class_class_id = ctx
        .ensure_class_initialized("java/lang/Class")
        .unwrap_or(rustjvm_types::ClassId::new(0));
    // Match the real-JDK java.lang.Class field count (19 slots).  Use the
    // loaded class's layout when available so our allocation isn't smaller
    // than what bytecode expects.
    let num_fields = {
        let n = ctx.class_num_total_fields(class_class_id);
        if n >= 19 { n } else { 19 }
    };
    let mirror = ctx.alloc_object(class_class_id, num_fields);
    ctx.set_field(mirror, 0, Value::Object(None));
    let name_str = ctx.create_string(name);
    ctx.set_field(mirror, 1, Value::Object(Some(name_str)));
    ctx.set_field(mirror, 12, Value::Int(0)); // classRedefinedCount
    mirror
}

/// Parse a method descriptor into (parameter type descriptors, return type descriptor).
///
/// e.g. "(ILjava/lang/String;)V" → (["I", "Ljava/lang/String;"], "V")
pub(crate) fn parse_descriptor_param_and_return(desc: &str) -> (Vec<String>, String) {
    let mut params = Vec::new();
    // Tolerant: an empty / malformed descriptor (no leading `(`) yields an
    // empty param list and `"V"` return — the same shape Spring's
    // `SerializableTypeWrapper` proxy handler expects when it has no
    // descriptor cached for an internally-synthesised method.
    if desc.is_empty() || !desc.starts_with('(') {
        return (params, "V".to_string());
    }
    let inner = &desc[1..]; // skip '('
    let mut i = 0;
    let bytes = inner.as_bytes();
    while i < bytes.len() && bytes[i] != b')' {
        let start = i;
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // skip ';'
                params.push(inner[start..i].to_string());
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
                    i += 1; // primitive array element
                }
                params.push(inner[start..i].to_string());
            }
            _ => {
                i += 1;
                params.push(inner[start..i].to_string());
            }
        }
    }
    // Skip ')' to get return type
    let ret_start = i + 1; // after ')'
    let ret_type = inner[ret_start..].to_string();
    (params, ret_type)
}

/// Box a VM Value into a wrapper object for reflection returns.
///
/// e.g. Value::Int(42) with type "I" → Integer.valueOf(42) object
pub(crate) fn box_value(ctx: &mut dyn NativeContext, value: Value, type_desc: &str) -> Value {
    match type_desc {
        "I" => {
            let obj = alloc_wrapper(ctx, "java/lang/Integer");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "J" => {
            let obj = alloc_wrapper(ctx, "java/lang/Long");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "F" => {
            let obj = alloc_wrapper(ctx, "java/lang/Float");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "D" => {
            let obj = alloc_wrapper(ctx, "java/lang/Double");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "Z" => {
            let obj = alloc_wrapper(ctx, "java/lang/Boolean");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "B" => {
            let obj = alloc_wrapper(ctx, "java/lang/Byte");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "S" => {
            let obj = alloc_wrapper(ctx, "java/lang/Short");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "C" => {
            let obj = alloc_wrapper(ctx, "java/lang/Character");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "V" => Value::Object(None), // void → null
        _ => value,                 // already an object reference
    }
}

/// Unbox a wrapper object to a primitive Value.
///
/// e.g. Integer object → Value::Int(42)
pub(crate) fn unbox_value(ctx: &dyn NativeContext, obj: rustjvm_types::ObjectRef) -> Value {
    let class_id = ctx.class_id_of_object(obj);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    match class_name.as_str() {
        "java/lang/Integer"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Character"
        | "java/lang/Boolean" => ctx.get_field(obj, 0),
        "java/lang/Long" => ctx.get_field(obj, 0),
        "java/lang/Float" => ctx.get_field(obj, 0),
        "java/lang/Double" => ctx.get_field(obj, 0),
        _ => Value::Object(Some(obj)), // not a wrapper, return as-is
    }
}

/// Unbox a Value::Object to a primitive based on the expected descriptor.
///
/// If the value is Object(None) (null), returns a default for the type.
/// If the value is already the right primitive type, returns it as-is.
pub(crate) fn unbox_arg(ctx: &dyn NativeContext, value: Value, expected_desc: &str) -> Value {
    match expected_desc {
        "I" | "Z" | "B" | "S" | "C" => match value {
            Value::Int(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Int(0),
        },
        "J" => match value {
            Value::Long(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Long(0),
        },
        "F" => match value {
            Value::Float(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Float(0.0),
        },
        "D" => match value {
            Value::Double(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Double(0.0),
        },
        _ => value, // object type, no unboxing needed
    }
}

// ---------------------------------------------------------------------------
// Strict reflective coercion — used by Method.invoke / Constructor.newInstance
// / Field.set to enforce IllegalArgumentException on type mismatches, as
// specified by JLS §15.12.4.2 and `java.lang.reflect` Javadoc.
// ---------------------------------------------------------------------------

/// Test whether a class name (slash-form, e.g. "java/lang/Integer") refers
/// to the wrapper for a given primitive descriptor.
fn wrapper_matches_primitive(wrapper: &str, prim_desc: &str) -> bool {
    matches!(
        (prim_desc, wrapper),
        ("I", "java/lang/Integer")
            | ("J", "java/lang/Long")
            | ("F", "java/lang/Float")
            | ("D", "java/lang/Double")
            | ("Z", "java/lang/Boolean")
            | ("B", "java/lang/Byte")
            | ("S", "java/lang/Short")
            | ("C", "java/lang/Character")
    )
}

/// Widening-primitive-conversion rules (JLS §5.1.2) for reflective
/// `Field.setInt` / `Method.invoke` coercion. Returns true iff a value
/// whose runtime primitive tag is `src` can be silently widened to `dst`.
fn widening_allowed(src: &str, dst: &str) -> bool {
    if src == dst {
        return true;
    }
    match (src, dst) {
        // byte → short, int, long, float, double
        ("B", "S" | "I" | "J" | "F" | "D") => true,
        // short → int, long, float, double
        ("S", "I" | "J" | "F" | "D") => true,
        // char → int, long, float, double
        ("C", "I" | "J" | "F" | "D") => true,
        // int → long, float, double
        ("I", "J" | "F" | "D") => true,
        // long → float, double
        ("J", "F" | "D") => true,
        // float → double
        ("F", "D") => true,
        _ => false,
    }
}

/// Infer the primitive descriptor for a wrapper class name.
fn wrapper_to_prim_desc(wrapper: &str) -> Option<&'static str> {
    Some(match wrapper {
        "java/lang/Integer" => "I",
        "java/lang/Long" => "J",
        "java/lang/Float" => "F",
        "java/lang/Double" => "D",
        "java/lang/Boolean" => "Z",
        "java/lang/Byte" => "B",
        "java/lang/Short" => "S",
        "java/lang/Character" => "C",
        _ => return None,
    })
}

/// Convert a primitive-bearing wrapper value to `Value::Int/Long/Float/Double`
/// after widening from `src_prim` to `dst_prim`. Returns None when the
/// requested widening is not a valid primitive conversion.
fn widen_primitive_value(value: Value, src_prim: &str, dst_prim: &str) -> Option<Value> {
    if !widening_allowed(src_prim, dst_prim) {
        return None;
    }
    // Extract the underlying numeric bits as i64/f64 so we can widen uniformly.
    let (as_long, as_double): (Option<i64>, Option<f64>) = match value {
        Value::Int(v) => (Some(v as i64), None),
        Value::Long(v) => (Some(v), None),
        Value::Float(v) => (None, Some(v as f64)),
        Value::Double(v) => (None, Some(v)),
        _ => (None, None),
    };
    Some(match dst_prim {
        "B" | "S" | "C" | "I" | "Z" => {
            let v = as_long.or_else(|| as_double.map(|d| d as i64))?;
            Value::Int(v as i32)
        }
        "J" => {
            let v = as_long.or_else(|| as_double.map(|d| d as i64))?;
            Value::Long(v)
        }
        "F" => {
            let v = as_double.or_else(|| as_long.map(|l| l as f64))?;
            Value::Float(v as f32)
        }
        "D" => {
            let v = as_double.or_else(|| as_long.map(|l| l as f64))?;
            Value::Double(v)
        }
        _ => return None,
    })
}

/// Strict version of `unbox_arg`: coerces an incoming Object reference to
/// the primitive expected by `expected_desc`, validating that the wrapper
/// class matches (possibly after widening). On mismatch throws
/// `IllegalArgumentException` as per `java.lang.reflect.Method.invoke`.
///
/// For reference-typed parameters, performs an assignability check against
/// `expected_desc`.
///
/// `context` is a short operation label used to construct the exception
/// message (e.g. "argument" or "Field.set").
pub(crate) fn coerce_arg_strict(
    ctx: &dyn NativeContext,
    value: Value,
    expected_desc: &str,
    context: &str,
) -> Result<Value, MethodCallFailed> {
    // WP2.1-field — operand-stack tag-erasure recovery for J/D.
    //
    // `CompactValue::to_value()` on an untagged 64-bit slot cannot tell
    // a small-magnitude long from a denormal double — both round-trip
    // identical bits.  When the operand stack pops a J-typed slot whose
    // bit pattern doesn't match a NaN-tagged subform, the resulting
    // `Value` is tagged `Double` even though the slot holds a long
    // (and vice-versa for a D slot whose bits collide with a tagged
    // subform).  See `types/src/compact_value.rs::to_value` and the
    // descriptor-aware sibling `decode_by_descriptor`.
    //
    // For typed setters, the method descriptor tells us the declared
    // type — reinterpret the bits accordingly so widening checks below
    // see the right tag.  This sits at the native edge so the
    // VM-internal call-frame plumbing stays untouched.
    let value = match (expected_desc, value) {
        ("J", Value::Double(d)) => Value::Long(d.to_bits() as i64),
        ("D", Value::Long(n)) => Value::Double(f64::from_bits(n as u64)),
        (_, v) => v,
    };
    match expected_desc {
        // Primitive expected
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C" => {
            match value {
                Value::Int(_) if matches!(expected_desc, "I" | "Z" | "B" | "S" | "C") => {
                    // boolean/byte/short/char/int share the same VM stack
                    // representation (Value::Int) — the caller already holds
                    // a legal payload for any of these slots, so pass it
                    // through without re-checking widening. Typed setter
                    // natives (e.g. setByte) mask the input to the field's
                    // storage width before reaching here.
                    Ok(value)
                }
                Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                    // Cross-category coercion (e.g. int → long, int → double):
                    // honour JLS §5.1.2 widening; narrowing is rejected.
                    let src = primitive_tag_of(value);
                    widen_primitive_value(value, src, expected_desc)
                        .ok_or_else(|| illegal_arg_exc(
                            format!(
                                "{context}: cannot convert {src} to {expected_desc}"
                            ),
                        ))
                }
                Value::Object(Some(obj)) => {
                    let wrapper_cid = ctx.class_id_of_object(obj);
                    let wrapper_name = ctx
                        .class_name_of_id(wrapper_cid)
                        .unwrap_or_default();
                    let src_prim = wrapper_to_prim_desc(&wrapper_name)
                        .ok_or_else(|| illegal_arg_exc(
                            format!(
                                "{context}: expected primitive {expected_desc}, got {wrapper_name}"
                            ),
                        ))?;
                    if !wrapper_matches_primitive(&wrapper_name, expected_desc)
                        && !widening_allowed(src_prim, expected_desc)
                    {
                        return Err(illegal_arg_exc(format!(
                            "{context}: cannot convert {wrapper_name} to {expected_desc}"
                        )));
                    }
                    // WP2.2 fix: read the wrapper's `value` field by name.
                    // Slot 0 is unreliable when the real JDK Byte/Short/Integer
                    // class has inherited fields ahead of `value` in the layout
                    // (Object header / Number padding). `get_field_by_name`
                    // resolves the descriptor-typed slot regardless of layout.
                    let by_name = ctx.get_field_by_name(obj, "value");
                    let by_slot0 = ctx.get_field(obj, 0);
                    let raw = match by_name {
                        Value::Object(None) => by_slot0,
                        v => v,
                    };
                    widen_primitive_value(raw, src_prim, expected_desc).ok_or_else(
                        || illegal_arg_exc(format!(
                            "{context}: cannot widen {src_prim} to {expected_desc}"
                        )),
                    )
                }
                Value::Object(None) => Err(illegal_arg_exc(format!(
                    "{context}: null argument not assignable to primitive {expected_desc}"
                ))),
                _ => Err(illegal_arg_exc(format!(
                    "{context}: unexpected VM value for primitive {expected_desc}"
                ))),
            }
        }
        // Reference type expected — null and matching references are OK.
        // Full assignability (L-type subclass checks, array-of-array) are
        // handled by the VM on invoke via its verifier; we just ensure the
        // slot kind is an Object.
        _ => match value {
            Value::Object(_) => Ok(value),
            _ => Err(illegal_arg_exc(format!(
                "{context}: expected reference ({expected_desc}), got primitive"
            ))),
        },
    }
}

fn primitive_tag_of(v: Value) -> &'static str {
    match v {
        Value::Int(_) => "I",
        Value::Long(_) => "J",
        Value::Float(_) => "F",
        Value::Double(_) => "D",
        _ => "?",
    }
}

/// Build an `IllegalArgumentException` VmError carrying `msg`.
pub(crate) fn illegal_arg_exc(msg: String) -> MethodCallFailed {
    rustjvm_types::error::RuntimeError::IllegalArgumentException { message: msg }.into()
}

/// Wrap a propagated Java exception from a reflective call into
/// `java.lang.reflect.InvocationTargetException(cause)`, matching HotSpot's
/// behaviour for `Method.invoke` and `Constructor.newInstance`.
///
/// If `failure` is an `InternalError` (VM bug or non-Java runtime error),
/// propagate it unchanged — wrapping would hide the defect.
///
/// If `failure` is an `ExceptionThrown`, allocate a new
/// `InvocationTargetException`, set its `cause` / target via Throwable's
/// `<init>(Throwable)` constructor, and return an `ExceptionThrown` holding
/// that new wrapper. This gives catching code the standard
/// `InvocationTargetException -> getCause() -> original` idiom.
pub(crate) fn wrap_as_invocation_target_exception(
    ctx: &mut dyn NativeContext,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    let original = match failure {
        MethodCallFailed::ExceptionThrown(obj) => obj,
        other => return other,
    };

    // Attempt to allocate and initialise
    // `java.lang.reflect.InvocationTargetException(Throwable)`. If any step
    // fails, fall back to the original exception rather than masking it.
    let target_class = "java/lang/reflect/InvocationTargetException";
    let new_result = ctx.new_object(target_class);
    let wrapper = match new_result {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        _ => return MethodCallFailed::ExceptionThrown(original),
    };

    // Preferred path: InvocationTargetException(Throwable target).
    let init_result = ctx.invoke(
        target_class,
        "<init>",
        "(Ljava/lang/Throwable;)V",
        &[
            Value::Object(Some(wrapper)),
            Value::Object(Some(original)),
        ],
    );
    match init_result {
        Ok(_) => MethodCallFailed::ExceptionThrown(wrapper),
        // If the two-arg constructor is unavailable, fall back to the
        // original — still catchable by Java code, just not wrapped.
        Err(_) => MethodCallFailed::ExceptionThrown(original),
    }
}

// ---------------------------------------------------------------------------
// setAccessible implementations
// ---------------------------------------------------------------------------

/// Shared body of `{Field,Method,Constructor}.setAccessible(boolean)`.
///
/// When `flag == true` this enforces JEP 403 strong encapsulation:
///
///   1. Read the declaring class mirror from field 0 of the AccessibleObject.
///   2. Resolve the caller class by walking the Java stack, skipping
///      reflection-internal frames.
///   3. Delegate to `NativeContext::check_deep_reflection_access` for the
///      `accessor → target` edge.
///   4. On failure, throw `java.lang.reflect.InaccessibleObjectException`
///      with a message identifying the denied module/package — matching
///      HotSpot's message format closely enough to be recognizable.
///
/// When `flag == false` the check is skipped (clearing the override cannot
/// fail).
fn set_accessible_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    flag_field_index: usize,
    member_label: &'static str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None), // Java would have NPE'd earlier — be lenient
    };
    let flag = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Only the `true` case needs the deep-reflection check.
    if flag != 0 {
        // Field 0 on Field/Method/Constructor is the declaring-class mirror.
        let declaring_mirror = match ctx.get_field(this, 0) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        };
        let target_class_name =
            declaring_mirror.and_then(|m| mirror_class_name(ctx, m));

        if let Some(target_class_name) = target_class_name {
            // JEP 403: setAccessible(true) is where the check is paid,
            // so we pass accessible_override=false even though the caller
            // is trying to *become* accessible.
            if let Err(msg) =
                check_reflection_module_access(ctx, &target_class_name, false)
            {
                return Err(rustjvm_types::error::RuntimeError::InaccessibleObjectException {
                    message: format!(
                        "Unable to make {member_label} accessible: {msg} \
                         (use --add-opens to grant access)"
                    ),
                }
                .into());
            }
        }
    }

    ctx.set_field(this, flag_field_index, Value::Int(flag));
    Ok(None)
}

/// Field.setAccessible(boolean) — writes the accessible flag.
/// Throws `InaccessibleObjectException` when the caller's module is not
/// granted deep-reflection access to the declaring class's package.
///
/// Unlike Method/Constructor (whose accessible flag we stash at a fixed
/// synthetic slot), Field uses the real JDK layout plus an extra slot,
/// so the flag is persisted via [`write_field_accessible`] and also
/// mirrored onto the JDK `override` inherited field.
pub(crate) fn native_field_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let flag = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    if flag != 0 {
        let target_class_name = match ctx.get_field_by_name(this, "clazz") {
            Value::Object(Some(m)) => mirror_class_name(ctx, m),
            _ => None,
        };
        if let Some(target_class_name) = target_class_name {
            if let Err(msg) =
                check_reflection_module_access(ctx, &target_class_name, false)
            {
                return Err(rustjvm_types::error::RuntimeError::InaccessibleObjectException {
                    message: format!(
                        "Unable to make field accessible: {msg} \
                         (use --add-opens to grant access)"
                    ),
                }
                .into());
            }
        }
    }

    write_field_accessible(ctx, this, flag != 0);
    // Also mirror to the JDK `override` inherited field so Java-side code
    // that consults `AccessibleObject.override` directly agrees.
    ctx.set_field_by_name(this, "override", Value::Int(flag));
    Ok(None)
}

/// Method.setAccessible(boolean) — sets the accessible flag.
/// Throws `InaccessibleObjectException` when the caller's module is not
/// granted deep-reflection access to the declaring class's package.
///
/// C6: accessible flag lives in a RustJVM extra slot (not a real JDK
/// field); the declaring-class mirror is read via `get_field_by_name`.
pub(crate) fn native_method_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    set_method_like_accessible_impl(ctx, args, "method", |c, o, v| {
        write_method_accessible(c, o, v)
    })
}

/// Constructor.setAccessible(boolean) — sets the accessible flag.
/// Throws `InaccessibleObjectException` when the caller's module is not
/// granted deep-reflection access to the declaring class's package.
///
/// C6: Constructor accessible flag lives in a RustJVM extra slot (not a
/// real JDK field); the declaring-class mirror is read via
/// `get_field_by_name`.
pub(crate) fn native_constructor_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    set_method_like_accessible_impl(ctx, args, "constructor", |c, o, v| {
        write_constructor_accessible(c, o, v)
    })
}

/// Shared setAccessible helper for Method / Constructor (both go through
/// extra-slot storage for the accessible flag after the C6 layout fix).
fn set_method_like_accessible_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    member_label: &'static str,
    mut write_flag: impl FnMut(&mut dyn NativeContext, rustjvm_types::ObjectRef, bool),
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let flag = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    if flag != 0 {
        let declaring_mirror = match ctx.get_field_by_name(this, "clazz") {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        };
        let target_class_name =
            declaring_mirror.and_then(|m| mirror_class_name(ctx, m));
        if let Some(target_class_name) = target_class_name {
            if let Err(msg) =
                check_reflection_module_access(ctx, &target_class_name, false)
            {
                return Err(rustjvm_types::error::RuntimeError::InaccessibleObjectException {
                    message: format!(
                        "Unable to make {member_label} accessible: {msg} \
                         (use --add-opens to grant access)"
                    ),
                }
                .into());
            }
        }
    }

    write_flag(ctx, this, flag != 0);
    // Mirror the JDK inherited `override` field as well (JEP 403).
    ctx.set_field_by_name(this, "override", Value::Int(flag));
    Ok(None)
}

// ---------------------------------------------------------------------------
// java.lang.reflect.Field — object layout and natives
// ---------------------------------------------------------------------------

/// Number of "extra" slots appended after the JDK Field layout to hold
/// RustJVM-specific metadata that doesn't exist on real JDK Field:
///   +0 → String (raw descriptor, e.g. "J" or "Ljava/lang/String;")
///   +1 → Int (RustJVM slot_index — absolute heap index for instance
///            fields, or field_index for static fields)
///   +2 → Int (accessible flag, 0 or 1)
const FIELD_EXTRA_SLOTS: usize = 3;
const FIELD_EXTRA_OFFSET_DESC: usize = 0;
const FIELD_EXTRA_OFFSET_RJ_SLOT: usize = 1;
const FIELD_EXTRA_OFFSET_ACCESSIBLE: usize = 2;

/// Legacy synthetic Field width — kept as a floor so older code paths
/// that still read slots 0..=6 directly don't stumble over a too-small
/// heap object. The real JDK layout is always ≥ 14, so in practice this
/// floor is only meaningful for synthetic stub classes where
/// `class_num_total_fields` returns 0.
const FIELD_NUM_FIELDS_LEGACY_FLOOR: usize = 7;
/// Legacy alias kept for the test-helper `make_field_mirror` which still
/// uses the synthetic 7-slot layout directly (the mock `NativeContext`
/// doesn't populate a real class-hierarchy so `get_field_by_name` would
/// fall back to slot 0 — instead the tests index by the synthetic slots).
#[cfg(test)]
const FIELD_NUM_FIELDS: usize = FIELD_NUM_FIELDS_LEGACY_FLOOR;

/// Helper: absolute slot for the first extra-metadata slot on a Field obj.
fn field_extra_base(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    core::cmp::max(FIELD_NUM_FIELDS_LEGACY_FLOOR, ctx.class_num_total_fields(class_id))
}

/// Create a Field reflection object from metadata.
///
/// C5 fix: Previously the object was allocated with only 7 slots and the
/// synthetic layout was written at slots 0..=6 by index. But
/// `java.lang.reflect.Field` extends `AccessibleObject`, so its real JDK
/// instance-field layout puts `clazz` at absolute slot 2, `slot` at 3,
/// `name` at 4, `type` at 5, `modifiers` at 6, etc. When Java-layer code
/// (e.g. `MethodHandles.Lookup.unreflectField`) did `Getfield clazz` /
/// `Getfield modifiers`, the interpreter resolved those real JDK offsets
/// and read our synthetic values — so `modifiers` came back as our
/// `slot_index` and `isStatic()` silently returned false, producing the
/// `expected a static field: ... from class java.lang.Object (null)`
/// IllegalAccessException for every `unreflectGetter(serialVersionUID)`.
///
/// Fix: populate the real JDK-named fields via `set_field_by_name` and
/// store RustJVM-specific metadata (descriptor string, slot_index,
/// accessible flag) in extra slots appended *after* the JDK layout. All
/// native readers go via `set_field_by_name` / `get_field_by_name` for
/// JDK fields and via the extra-slot helpers for our metadata.
pub(crate) fn create_field_object(
    ctx: &mut dyn NativeContext,
    meta: &FieldMetadata,
) -> rustjvm_types::ObjectRef {
    let class_id = ctx.ensure_class_initialized("java/lang/reflect/Field")
        .unwrap_or(ClassId::new(0));

    // Allocate JDK-layout width + our extra metadata slots.
    let jdk_layout_fields = ctx.class_num_total_fields(class_id);
    let base = core::cmp::max(FIELD_NUM_FIELDS_LEGACY_FLOOR, jdk_layout_fields);
    let num_fields = base + FIELD_EXTRA_SLOTS;
    let obj = ctx.alloc_object(class_id, num_fields);

    let class_mirror = ctx.get_class_mirror(meta.declaring_class_id);
    let name_str = ctx.create_string(&meta.name);
    let type_mirror = descriptor_to_class_mirror(ctx, &meta.descriptor);
    let desc_str = ctx.create_string(&meta.descriptor);

    // --- Real JDK Field layout (visible to Java bytecode via Getfield) ---
    ctx.set_field_by_name(obj, "clazz", Value::Object(Some(class_mirror)));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(obj, "type", Value::Object(Some(type_mirror)));
    ctx.set_field_by_name(obj, "modifiers", Value::Int(meta.access_flags as i32));
    ctx.set_field_by_name(obj, "slot", Value::Int(meta.slot_index as i32));
    // JDK's `trustedFinal` is JVM-internal; default to false.
    ctx.set_field_by_name(obj, "trustedFinal", Value::Int(0));

    // --- RustJVM extra metadata (append after JDK layout) ---
    ctx.set_field(obj, base + FIELD_EXTRA_OFFSET_DESC, Value::Object(Some(desc_str)));
    ctx.set_field(obj, base + FIELD_EXTRA_OFFSET_RJ_SLOT, Value::Int(meta.slot_index as i32));
    ctx.set_field(obj, base + FIELD_EXTRA_OFFSET_ACCESSIBLE, Value::Int(0));


    obj
}

/// Helper: read the static-flag, declaring ClassId, slot_index, and descriptor
/// from a Field reflection object's fields.
///
/// Uses `get_field_by_name` for the JDK-layout fields so the read lands on
/// the right slot regardless of class hierarchy offsets, and falls back to
/// the RustJVM extra-slot for the descriptor and slot_index (which don't
/// exist on real JDK Field).
pub(crate) fn read_field_meta(
    ctx: &dyn NativeContext,
    field_obj: rustjvm_types::ObjectRef,
) -> (bool, rustjvm_types::ClassId, usize, String) {
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_static = (modifiers & ACC_STATIC) != 0;

    let class_id = {
        let mirror = match ctx.get_field_by_name(field_obj, "clazz") {
            Value::Object(Some(m)) => m,
            _ => {
                return (
                    false,
                    rustjvm_types::ClassId::new(0),
                    0,
                    String::new(),
                );
            }
        };
        mirror_class_id(ctx, mirror).unwrap_or(rustjvm_types::ClassId::new(0))
    };

    // `slot_index` is stashed in our RustJVM extra slots (not a real JDK
    // field — the JDK `slot` field has different semantics).
    let field_class_id = {
        // Determine the object's class to locate the extra-slot base.
        // We don't have a direct `class_id_of` on `NativeContext`; fall
        // back to reading our custom `slot` field by name first, which
        // we intentionally populate both in JDK layout AND in the extra
        // slot. If neither is available we return 0.
        rustjvm_types::ClassId::new(0)
    };
    let _ = field_class_id; // suppress unused

    // Prefer the extra-slot int (reliable for static fields, where JDK's
    // `slot` is an opaque vmindex we don't control). Fall back to JDK's
    // `slot` field if the extra slot isn't populated.
    let slot = match read_field_rj_slot(ctx, field_obj) {
        Some(v) => v,
        None => match ctx.get_field_by_name(field_obj, "slot") {
            Value::Int(v) => v as usize,
            _ => 0,
        },
    };

    let descriptor = match read_field_descriptor(ctx, field_obj) {
        Some(s) => s,
        None => String::new(),
    };

    (is_static, class_id, slot, descriptor)
}

/// Read the RustJVM-specific `slot_index` extra slot from a Field object.
/// Returns `None` if the extra slot is unreadable or zero-uninitialised.
fn read_field_rj_slot(
    ctx: &dyn NativeContext,
    field_obj: rustjvm_types::ObjectRef,
) -> Option<usize> {
    let class_id = ctx.class_id_of_object(field_obj);
    let base = field_extra_base(ctx, class_id);
    let val_at_base = ctx.get_field(field_obj, base + FIELD_EXTRA_OFFSET_RJ_SLOT);
    match val_at_base {
        Value::Int(v) => Some(v as usize),
        // T19.H1 — defensive scan. Historically the base used at creation
        // time (Field.class not yet linked → class_num_total_fields returns
        // 0 → base=7) could differ from the base used at read time
        // (Field.class now linked → base=14). Rather than always storing at
        // the larger fixed base, we scan the likely extra-slot range for a
        // plausible slot_index value. Finding a non-negative Int in the
        // extra-slot range beats livelocking every CAS loop that depends on
        // Unsafe.objectFieldOffset(Field).
        _ => {
            // The Field heap object is allocated as
            //   num_fields = max(7, jdk_layout_fields) + FIELD_EXTRA_SLOTS
            // so any of the candidate bases below point at valid storage.
            // Walk the two known bases (7 for the legacy floor and the
            // current JDK layout width) and return the first Int we find.
            for candidate_base in [FIELD_NUM_FIELDS_LEGACY_FLOOR, 14, 15, 16] {
                if let Value::Int(v) = ctx.get_field(
                    field_obj,
                    candidate_base + FIELD_EXTRA_OFFSET_RJ_SLOT,
                ) {
                    if v >= 0 {
                        return Some(v as usize);
                    }
                }
            }
            None
        }
    }
}

/// Read the RustJVM-specific `descriptor` extra slot from a Field object.
fn read_field_descriptor(
    ctx: &dyn NativeContext,
    field_obj: rustjvm_types::ObjectRef,
) -> Option<String> {
    let class_id = ctx.class_id_of_object(field_obj);
    let base = field_extra_base(ctx, class_id);
    match ctx.get_field(field_obj, base + FIELD_EXTRA_OFFSET_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the RustJVM-specific `accessible` extra slot from a Field object.
fn read_field_accessible(
    ctx: &dyn NativeContext,
    field_obj: rustjvm_types::ObjectRef,
) -> bool {
    let class_id = ctx.class_id_of_object(field_obj);
    let base = field_extra_base(ctx, class_id);
    match ctx.get_field(field_obj, base + FIELD_EXTRA_OFFSET_ACCESSIBLE) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

/// Write the RustJVM-specific `accessible` extra slot on a Field object.
pub(crate) fn write_field_accessible(
    ctx: &mut dyn NativeContext,
    field_obj: rustjvm_types::ObjectRef,
    value: bool,
) {
    let class_id = ctx.class_id_of_object(field_obj);
    let base = field_extra_base(ctx, class_id);
    ctx.set_field(
        field_obj,
        base + FIELD_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(if value { 1 } else { 0 }),
    );
}

/// Public wrapper used by `lang_reflect::native_field_try_set_accessible`.
/// Same as `write_field_accessible` but kept as a stable cross-module name.
pub(crate) fn write_field_accessible_external(
    ctx: &mut dyn NativeContext,
    field_obj: rustjvm_types::ObjectRef,
    value: bool,
) {
    write_field_accessible(ctx, field_obj, value);
}

// --- Field getters (simple field reads) ---

pub(crate) fn native_field_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "name")))
}

pub(crate) fn native_field_get_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "type")))
}

pub(crate) fn native_field_get_modifiers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field_by_name(this, "modifiers")))
}

pub(crate) fn native_field_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "clazz")))
}

// --- Field.get(Object) / Field.set(Object, Object) ---

pub(crate) fn native_field_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field.get: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };

    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control: read modifiers from JDK layout, accessible from
    // the RustJVM extra slot.
    let modifiers = match ctx.get_field_by_name(this, "modifiers") { Value::Int(v) => v, _ => 0 };
    let accessible = read_field_accessible(ctx, this);
    check_access(modifiers, accessible, &format!("Field.get({})", descriptor))?;
    // NEW-19: module-level opens check (JPMS)
    enforce_module_check_on_field(ctx, this, accessible, "Field.get")?;

    // WP2.1-field — volatile-aware read fence: matches what the JDK does
    // internally via `Unsafe.getReferenceVolatile`/`getIntVolatile`. No-op
    // for non-volatile fields so the plain-read path stays cheap.
    volatile_load_fence(modifiers);

    let raw_value = if is_static {
        ctx.get_static_field(class_id, slot)
    } else {
        let recv = receiver.ok_or_else(|| rustjvm_types::error::RuntimeError::NullPointerException {
            message: Some("Field.get: null receiver for instance field".to_string()),
        })?;
        ctx.get_field(recv, slot)
    };

    // Box primitive values for the generic Object return
    let result = box_value(ctx, raw_value, &descriptor);
    Ok(Some(result))
}

pub(crate) fn native_field_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field.set: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let new_value = args.get(2).copied().unwrap_or(Value::Object(None));

    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control: read modifiers from JDK layout, accessible from
    // the RustJVM extra slot.
    let modifiers = match ctx.get_field_by_name(this, "modifiers") { Value::Int(v) => v, _ => 0 };
    let accessible = read_field_accessible(ctx, this);
    check_access(modifiers, accessible, &format!("Field.set({})", descriptor))?;
    // WP2.1-field — final-field write check (must run AFTER access check
    // so the more specific error message wins on a public-final field).
    check_final_for_set(modifiers, accessible, &format!("Field.set({})", descriptor))?;
    // NEW-19: module-level opens check (JPMS)
    enforce_module_check_on_field(ctx, this, accessible, "Field.set")?;

    // Strictly coerce the value if the field expects a primitive (including
    // widening); this raises IllegalArgumentException if the wrapper type
    // cannot be narrowed/widened to the target primitive per JLS §5.1.2.
    let coerced = coerce_arg_strict(ctx, new_value, &descriptor, "Field.set")?;

    // WP2.1-field — volatile-aware write fences (no-op for non-volatile).
    volatile_store_fence_pre(modifiers);
    if is_static {
        ctx.set_static_field(class_id, slot, coerced);
    } else {
        let recv = receiver.ok_or_else(|| rustjvm_types::error::RuntimeError::NullPointerException {
            message: Some("Field.set: null receiver for instance field".to_string()),
        })?;
        ctx.set_field(recv, slot, coerced);
    }
    volatile_store_fence_post(modifiers);
    Ok(None)
}

// --- Typed Field getters (getInt, getLong, getFloat, getDouble, getBoolean) ---

fn field_get_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Value, rustjvm_types::error::MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field typed getter: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let (is_static, class_id, slot, _descriptor) = read_field_meta(ctx, this);

    // Access control
    let modifiers = match ctx.get_field_by_name(this, "modifiers") { Value::Int(v) => v, _ => 0 };
    let accessible = read_field_accessible(ctx, this);
    check_access(modifiers, accessible, "Field typed getter")?;
    // NEW-19: module-level opens check (JPMS).
    // `enforce_module_check_from_mirror` takes the slot index of the
    // declaring-class mirror on the Field object; still 0 historically,
    // but the real JDK layout puts `clazz` elsewhere. Wrap with a helper.
    enforce_module_check_on_field(ctx, this, accessible, "Field typed getter")?;

    // WP2.1-field — volatile-aware read fence (no-op for non-volatile).
    volatile_load_fence(modifiers);

    if is_static {
        Ok(ctx.get_static_field(class_id, slot))
    } else {
        let recv = receiver.ok_or_else(|| rustjvm_types::error::RuntimeError::NullPointerException {
            message: Some("Field typed getter: null receiver for instance field".to_string()),
        })?;
        Ok(ctx.get_field(recv, slot))
    }
}

pub(crate) fn native_field_get_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Field.getInt accepts byte/short/char/int fields (all represented as
    // Value::Int on our stack) — but NOT long, float, or double, per
    // `java.lang.reflect.Field.getInt` javadoc (IllegalArgumentException on
    // non-int-compatible types).
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Int(_) => Ok(Some(val)),
        _ => Err(illegal_arg_exc(
            "Field.getInt: field type is not int-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_get_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Accepts byte/short/char/int/long (widening).
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Long(_) => Ok(Some(val)),
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Err(illegal_arg_exc(
            "Field.getLong: field type is not long-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_get_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Accepts byte/short/char/int/long/float (widening).
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Float(_) => Ok(Some(val)),
        Value::Int(v) => Ok(Some(Value::Float(v as f32))),
        Value::Long(v) => Ok(Some(Value::Float(v as f32))),
        _ => Err(illegal_arg_exc(
            "Field.getFloat: field type is not float-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_get_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Accepts every numeric primitive (widening to double).
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Double(_) => Ok(Some(val)),
        Value::Float(v) => Ok(Some(Value::Double(v as f64))),
        Value::Int(v) => Ok(Some(Value::Double(v as f64))),
        Value::Long(v) => Ok(Some(Value::Double(v as f64))),
        _ => Err(illegal_arg_exc(
            "Field.getDouble: field type is not double-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_get_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // getBoolean only accepts boolean fields (per javadoc — no widening).
    // We detect non-boolean fields by reading the descriptor alongside.
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field.getBoolean: null Field".to_string()),
            }
            .into())
        }
    };
    let (_, _, _, descriptor) = read_field_meta(ctx, this);
    if descriptor != "Z" {
        return Err(illegal_arg_exc(
            "Field.getBoolean: field type is not boolean".to_string(),
        ));
    }
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Int(v) => Ok(Some(Value::Int(if v != 0 { 1 } else { 0 }))),
        _ => Err(illegal_arg_exc(
            "Field.getBoolean: field type is not boolean".to_string(),
        )),
    }
}

// --- Typed Field setters (setInt, setLong, setFloat, setDouble, setBoolean) ---

fn field_set_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    new_value: Value,
) -> Result<(), rustjvm_types::error::MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field typed setter: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control
    let modifiers = match ctx.get_field_by_name(this, "modifiers") { Value::Int(v) => v, _ => 0 };
    let accessible = read_field_accessible(ctx, this);
    check_access(modifiers, accessible, "Field typed setter")?;
    // WP2.1-field — final-field write check (matches Field.set on the
    // generic `set(Object,Object)` path).
    check_final_for_set(modifiers, accessible, "Field typed setter")?;
    // NEW-19: module-level opens check (JPMS)
    enforce_module_check_on_field(ctx, this, accessible, "Field typed setter")?;

    // Narrow or widen the incoming primitive into whatever the field
    // actually holds. This catches `Field.setInt(...)` on a reference field
    // and the like, producing IllegalArgumentException as per javadoc.
    let coerced = coerce_arg_strict(ctx, new_value, &descriptor, "Field typed setter")?;

    // WP2.1-field — volatile-aware write fences (no-op for non-volatile).
    volatile_store_fence_pre(modifiers);
    if is_static {
        ctx.set_static_field(class_id, slot, coerced);
    } else {
        let recv = receiver.ok_or_else(|| rustjvm_types::error::RuntimeError::NullPointerException {
            message: Some("Field typed setter: null receiver for instance field".to_string()),
        })?;
        ctx.set_field(recv, slot, coerced);
    }
    volatile_store_fence_post(modifiers);
    Ok(())
}

pub(crate) fn native_field_set_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Int(0));
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

pub(crate) fn native_field_set_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Long(0));
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

pub(crate) fn native_field_set_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Float(0.0));
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

pub(crate) fn native_field_set_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Double(0.0));
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

pub(crate) fn native_field_set_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.get(2).copied().unwrap_or(Value::Int(0));
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

// --- Remaining typed Field getters: byte / short / char ---

pub(crate) fn native_field_get_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Int(v) => Ok(Some(Value::Int((v as i8) as i32))),
        _ => Err(illegal_arg_exc(
            "Field.getByte: field type is not byte-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_get_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = field_get_raw(ctx, args)?;
    match val {
        Value::Int(v) => Ok(Some(Value::Int((v as i16) as i32))),
        _ => Err(illegal_arg_exc(
            "Field.getShort: field type is not short-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_get_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = field_get_raw(ctx, args)?;
    match val {
        // char is an unsigned 16-bit type stored in an int slot; clamp to u16.
        Value::Int(v) => Ok(Some(Value::Int((v as u16) as i32))),
        _ => Err(illegal_arg_exc(
            "Field.getChar: field type is not char-compatible".to_string(),
        )),
    }
}

pub(crate) fn native_field_set_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    let val = match raw {
        Value::Int(v) => Value::Int((v as i8) as i32),
        _ => {
            return Err(illegal_arg_exc(
                "Field.setByte: value is not byte-compatible".to_string(),
            ))
        }
    };
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

pub(crate) fn native_field_set_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    let val = match raw {
        Value::Int(v) => Value::Int((v as i16) as i32),
        _ => {
            return Err(illegal_arg_exc(
                "Field.setShort: value is not short-compatible".to_string(),
            ))
        }
    };
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

pub(crate) fn native_field_set_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    let val = match raw {
        Value::Int(v) => Value::Int((v as u16) as i32),
        _ => {
            return Err(illegal_arg_exc(
                "Field.setChar: value is not char-compatible".to_string(),
            ))
        }
    };
    field_set_raw(ctx, args, val)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Class.getDeclaredFields / getDeclaredField
// ---------------------------------------------------------------------------

pub(crate) fn native_class_get_declared_fields(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredFields on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // Primitive or array type — no declared fields
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let fields = ctx.declared_fields(class_id);
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), fields.len());
    for (i, meta) in fields.iter().enumerate() {
        let field_obj = create_field_object(ctx, meta);
        ctx.set_array_element(arr, i, Value::Object(Some(field_obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_declared_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredField on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredField: name is null".to_string()),
            }
            .into())
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NoSuchFieldException {
                field_name: target_name,
            }
            .into())
        }
    };

    let fields = ctx.declared_fields(class_id);
    for meta in &fields {
        if meta.name == target_name {
            let field_obj = create_field_object(ctx, meta);
            return Ok(Some(Value::Object(Some(field_obj))));
        }
    }

    Err(rustjvm_types::error::RuntimeError::NoSuchFieldException {
        field_name: target_name,
    }
    .into())
}

// ---------------------------------------------------------------------------
// java.lang.reflect.Method — object layout and natives
// ---------------------------------------------------------------------------

/// Number of "extra" slots appended after the JDK Method layout to hold
/// RustJVM-specific metadata that doesn't exist on real JDK Method:
///   +0 → String (raw descriptor, e.g. "(II)I")
///   +1 → Int    (parameter count — cached)
///   +2 → Int    (accessible flag, 0 or 1)
const METHOD_EXTRA_SLOTS: usize = 3;
const METHOD_EXTRA_OFFSET_DESC: usize = 0;
const METHOD_EXTRA_OFFSET_PARAM_COUNT: usize = 1;
const METHOD_EXTRA_OFFSET_ACCESSIBLE: usize = 2;

/// Legacy synthetic Method width — kept as a floor so the allocated
/// object is always large enough to host the synthetic writes made by
/// tests (via MockNativeContext which maps field names to these slots)
/// and to avoid underallocation when `class_num_total_fields` returns 0
/// (class not loaded yet).
// G2: bumped from 8 → 12 to accommodate the 4 additional non-null array
// fields populated by `create_method_object` (`exceptionTypes`,
// `annotations`, `parameterAnnotations`, `annotationDefault`). The mock
// `MockNativeContext` maps these names to slots 9..=12 (see
// `mock_jdk_field_slot`); the floor must be ≥ those slots so the heap
// allocation reserves room.
const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;

/// Public for tests that still index by a synthetic slot layout. Mirrors
/// the floor so tests that assume the 8-slot layout still have room.
#[cfg(test)]
const METHOD_NUM_FIELDS: usize = METHOD_NUM_FIELDS_LEGACY_FLOOR;

/// Helper: absolute slot for the first extra-metadata slot on a Method obj.
fn method_extra_base(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    core::cmp::max(
        METHOD_NUM_FIELDS_LEGACY_FLOOR,
        ctx.class_num_total_fields(class_id),
    )
}

/// C6 fix: Previously the Method object was allocated with only 8 slots
/// and the synthetic layout was written at slots 0..=7 by index. But real
/// JDK `java.lang.reflect.Method` inherits from
/// `java.lang.reflect.Executable` → `AccessibleObject`, so its declared
/// instance-field layout puts `clazz` at a higher absolute slot (not 0).
/// When real-JDK bytecode did `Getfield clazz` / `Getfield modifiers`
/// (e.g. `Method.isCallerSensitive` → `getDeclaringClass`), the
/// interpreter resolved those real JDK offsets and read our synthetic
/// values — so a `GETFIELD clazz` landed on a slot holding an
/// `Int(1)` residue, producing the
/// `expected object reference, got int(1)` crash observed in stream +
/// enum reflection paths (EnumMap.<init> → getEnumConstantsShared →
/// Method.invoke → isCallerSensitive → getDeclaringClass).
///
/// Fix: populate the real JDK-named fields via `set_field_by_name` and
/// store RustJVM-specific metadata (raw descriptor, cached parameter
/// count, accessible flag) in extra slots appended *after* the JDK
/// layout. All native readers go via `get_field_by_name` for JDK fields
/// and via the extra-slot helpers for our metadata.
pub(crate) fn create_method_object(
    ctx: &mut dyn NativeContext,
    meta: &MethodMetadata,
) -> rustjvm_types::ObjectRef {
    let class_id = ctx.ensure_class_initialized("java/lang/reflect/Method")
        .unwrap_or(ClassId::new(0));

    // Allocate JDK-layout width + our extra metadata slots.
    let jdk_layout_fields = ctx.class_num_total_fields(class_id);
    let base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, jdk_layout_fields);
    let num_fields = base + METHOD_EXTRA_SLOTS;
    let obj = ctx.alloc_object(class_id, num_fields);

    let class_mirror = ctx.get_class_mirror(meta.declaring_class_id);
    let name_str = ctx.create_string(&meta.name);

    // Parse descriptor for param types and return type
    let (param_descs, ret_desc) = parse_descriptor_param_and_return(&meta.descriptor);
    let ret_mirror = descriptor_to_class_mirror(ctx, &ret_desc);

    // Parameter type mirrors array.
    let param_arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), param_descs.len());
    for (i, pdesc) in param_descs.iter().enumerate() {
        let pmirror = descriptor_to_class_mirror(ctx, pdesc);
        ctx.set_array_element(param_arr, i, Value::Object(Some(pmirror)));
    }

    // G2: Always allocate non-null array fields. JDK 25 `Method` and its
    // parent `Executable` declare `exceptionTypes` (Class[]) and several
    // byte-array fields (`annotations`, `parameterAnnotations`,
    // `annotationDefault`). If these are left as the default `null`,
    // any caller doing `arr.length` or `arr.clone()` or even just
    // iterating will NPE — most notably ByteBuddy's
    // `JavaDispatcher.<clinit>` which does `arraylength` on a `Method`-
    // returned array. The real JDK guarantees these fields are non-null
    // (initialised by the `Method` constructor); we mirror that.
    //
    // WP2.1 — populate `exceptionTypes` from the JVMS §4.7.5 `Exceptions`
    // attribute when present, so `Method.getExceptionTypes()` (which the
    // JDK Java code implements by `return exceptionTypes.clone();`)
    // returns the actual throws-clause types instead of always-empty.
    let exception_names = ctx.method_exceptions(
        meta.declaring_class_id,
        &meta.name,
        &meta.descriptor,
    );
    let exception_arr = ctx.new_ref_array(
        rustjvm_types::ClassId::new(0),
        exception_names.len(),
    );
    for (i, name) in exception_names.iter().enumerate() {
        // Build a Class<T> mirror for each thrown checked exception.
        // We use `descriptor_to_class_mirror` with an L-form so that
        // the same code path that turns `Ljava/io/IOException;` into a
        // mirror handles class loading + caching consistently.
        let desc = format!("L{name};");
        let mirror = descriptor_to_class_mirror(ctx, &desc);
        ctx.set_array_element(exception_arr, i, Value::Object(Some(mirror)));
    }
    let empty_byte_arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 0);

    let desc_str = ctx.create_string(&meta.descriptor);

    // --- Real JDK Method layout (visible to Java bytecode via Getfield) ---
    ctx.set_field_by_name(obj, "clazz", Value::Object(Some(class_mirror)));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(obj, "returnType", Value::Object(Some(ret_mirror)));
    ctx.set_field_by_name(obj, "parameterTypes", Value::Object(Some(param_arr)));
    // G2 + WP2.1: exceptionTypes is a non-null Class[] populated from the
    // `Exceptions` class-file attribute (or empty if no throws clause).
    // `Method.getExceptionTypes()` does `exceptionTypes.clone()` — if this
    // were null, ByteBuddy / Mockito / Spring AOP clinit paths would NPE.
    ctx.set_field_by_name(obj, "exceptionTypes", Value::Object(Some(exception_arr)));
    ctx.set_field_by_name(obj, "modifiers", Value::Int(meta.access_flags as i32));
    // JDK's `slot` is an opaque vmindex we don't populate; default 0.
    ctx.set_field_by_name(obj, "slot", Value::Int(0));
    // `callerSensitive` is a byte cache; 0 means "not yet computed".
    ctx.set_field_by_name(obj, "callerSensitive", Value::Int(0));
    // G2: annotation byte-array fields — empty arrays, not null. JDK 25
    // `Method.getAnnotationBytes()` callers walk these without null
    // checks (e.g. `AnnotationParser.parseAnnotations` reads `arr.length`).
    ctx.set_field_by_name(obj, "annotations", Value::Object(Some(empty_byte_arr)));
    let empty_byte_arr2 = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 0);
    ctx.set_field_by_name(obj, "parameterAnnotations", Value::Object(Some(empty_byte_arr2)));
    let empty_byte_arr3 = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 0);
    ctx.set_field_by_name(obj, "annotationDefault", Value::Object(Some(empty_byte_arr3)));

    // --- RustJVM extra metadata (append after JDK layout) ---
    ctx.set_field(
        obj,
        base + METHOD_EXTRA_OFFSET_DESC,
        Value::Object(Some(desc_str)),
    );
    ctx.set_field(
        obj,
        base + METHOD_EXTRA_OFFSET_PARAM_COUNT,
        Value::Int(param_descs.len() as i32),
    );
    ctx.set_field(
        obj,
        base + METHOD_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(0),
    );

    obj
}

/// Read the RustJVM-specific raw descriptor extra slot from a Method object.
pub(crate) fn read_method_descriptor(
    ctx: &dyn NativeContext,
    method_obj: rustjvm_types::ObjectRef,
) -> Option<String> {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    match ctx.get_field(method_obj, base + METHOD_EXTRA_OFFSET_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the RustJVM-specific cached parameter count extra slot.
fn read_method_param_count(
    ctx: &dyn NativeContext,
    method_obj: rustjvm_types::ObjectRef,
) -> i32 {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    match ctx.get_field(method_obj, base + METHOD_EXTRA_OFFSET_PARAM_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    }
}

/// Read the RustJVM-specific accessible flag extra slot.
///
/// In real JDK 25, `Method.setAccessible(boolean)` is NOT a native — it's
/// inherited from `AccessibleObject.setAccessible(boolean)` which sets the
/// `override` field directly via Java putfield. When dispatch routes through
/// the Java method (or some other path that bypasses `native_method_set_accessible`),
/// the RustJVM extra slot stays at 0 even though `override == true`.
///
/// Therefore we consult BOTH:
///   1. The JDK-inherited `override` field (canonical bool, accessed via
///      `get_field_by_name` so it works whether stored as Int(0/1) or other
///      truthy encodings).
///   2. The RustJVM extra-slot fallback (kept for paths that only set the
///      extra slot — e.g. internal write helpers).
///
/// Either being truthy is enough to treat the Method as accessible.
fn read_method_accessible(
    ctx: &dyn NativeContext,
    method_obj: rustjvm_types::ObjectRef,
) -> bool {
    // Check the JDK `override` field first — this is what JDK 25's
    // AccessibleObject.setAccessible writes via Java bytecode.
    if let Value::Int(v) = ctx.get_field_by_name(method_obj, "override") {
        if v != 0 {
            return true;
        }
    }
    // Fall back to the RustJVM extra slot (set by our native setAccessible).
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    match ctx.get_field(method_obj, base + METHOD_EXTRA_OFFSET_ACCESSIBLE) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

/// Write the RustJVM-specific accessible flag extra slot on a Method object.
pub(crate) fn write_method_accessible(
    ctx: &mut dyn NativeContext,
    method_obj: rustjvm_types::ObjectRef,
    value: bool,
) {
    let class_id = ctx.class_id_of_object(method_obj);
    let base = method_extra_base(ctx, class_id);
    ctx.set_field(
        method_obj,
        base + METHOD_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(if value { 1 } else { 0 }),
    );
}

/// Public wrapper used by `lang_reflect::native_method_try_set_accessible`.
pub(crate) fn write_method_accessible_external(
    ctx: &mut dyn NativeContext,
    method_obj: rustjvm_types::ObjectRef,
    value: bool,
) {
    write_method_accessible(ctx, method_obj, value);
}

// --- Method getters ---

pub(crate) fn native_method_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "name")))
}

pub(crate) fn native_method_get_return_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "returnType")))
}

pub(crate) fn native_method_get_parameter_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "parameterTypes")))
}

pub(crate) fn native_method_get_modifiers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field_by_name(this, "modifiers")))
}

pub(crate) fn native_method_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "clazz")))
}

pub(crate) fn native_method_get_parameter_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(read_method_param_count(ctx, this))))
}

// --- Method.invoke ---

pub(crate) fn native_method_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Method.invoke: null Method".to_string()),
            }
            .into())
        }
    };

    // Read method metadata from the Method object via JDK field names
    // (C6: real-JDK layout differs from our synthetic slot indices).
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_static = (modifiers & ACC_STATIC) != 0;

    // Get declaring class name
    let declaring_mirror = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(m)) => m,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Method.invoke: no declaring class".to_string()),
            }
            .into())
        }
    };
    let class_name = mirror_class_name(ctx, declaring_mirror).unwrap_or_default();

    // Get method name
    let method_name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };

    // Access control: accessible flag lives in a RustJVM extra slot.
    let accessible = read_method_accessible(ctx, this);
    check_access(modifiers, accessible, &format!("Method.invoke: {}.{}", class_name, method_name))?;
    // NEW-19: module-level opens check (JPMS). When `accessible == true`
    // the override flag short-circuits the deep check (JEP 403).
    //
    // JEP 403/261 distinction: PUBLIC methods of EXPORTED packages need
    // only `exports`, not `opens`. Only enforce the deep check when the
    // method is non-public (ACC_PUBLIC = 0x0001) — that's the case where
    // setAccessible / opens is required.
    let is_public = (modifiers & 0x0001) != 0;
    if !is_public {
        if let Err(msg) = check_reflection_module_access(ctx, &class_name, accessible) {
            return Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
                message: format!("Method.invoke: {class_name}.{method_name}: {msg}"),
            }
            .into());
        }
    }

    // Get descriptor — stored in RustJVM extra slot (not a real JDK field).
    let descriptor = read_method_descriptor(ctx, this).unwrap_or_default();

    // Parse parameter types from descriptor
    let (param_descs, ret_desc) = parse_descriptor_param_and_return(&descriptor);

    // Extract receiver (args[1]) and arguments array (args[2])
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let args_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    // Build invocation arguments
    let mut invoke_args: Vec<Value> = Vec::new();

    if !is_static {
        // Instance method: receiver is first arg
        let recv = receiver.ok_or_else(|| rustjvm_types::error::RuntimeError::NullPointerException {
            message: Some("Method.invoke: null receiver for instance method".to_string()),
        })?;
        invoke_args.push(Value::Object(Some(recv)));
    }

    // Validate argument-array length matches parameter count (HotSpot throws
    // IllegalArgumentException when they differ — except when the target is
    // varargs and the last parameter receives the tail). This matches
    // `java.lang.reflect.Method.invoke` semantics.
    let actual_arg_count = match args_array {
        Some(arr) => ctx.array_length(arr),
        None => 0,
    };
    if actual_arg_count != param_descs.len() {
        return Err(illegal_arg_exc(format!(
            "Method.invoke: wrong number of arguments for {class_name}.{method_name}: \
             expected {}, got {}",
            param_descs.len(),
            actual_arg_count,
        )));
    }

    // Coerce each argument strictly, raising IllegalArgumentException on
    // type mismatch (per java.lang.reflect.Method.invoke javadoc).
    for (i, pdesc) in param_descs.iter().enumerate() {
        let arg_val = if let Some(arr) = args_array {
            ctx.get_array_element(arr, i)
        } else {
            Value::Object(None)
        };
        let coerced = coerce_arg_strict(ctx, arg_val, pdesc, "Method.invoke argument")?;
        invoke_args.push(coerced);
    }

    // Invoke the method. Any Java exception thrown by the callee must be
    // wrapped in InvocationTargetException per the Method.invoke contract.
    //
    // Per JLS §15.12.4.4 / `Method.invoke` Javadoc: "If the underlying method
    // is an instance method, it is invoked using dynamic method lookup ...
    // overriding based on the runtime type of the target object will occur."
    //
    // So for non-static, non-private, non-`<init>` instance methods we MUST
    // perform virtual dispatch on the receiver's runtime class — even when
    // the cached Method points at a concrete declaring class (e.g.
    // `Super.class.getDeclaredMethod("hello")` on a `SubB` receiver should
    // call `SubB.hello`, not `Super.hello`).
    //
    // Static methods, `<init>`, and private methods bypass virtual dispatch
    // and call the resolved class directly.
    const ACC_PRIVATE: i32 = 0x0002;
    let is_private = (modifiers & ACC_PRIVATE) != 0;
    let is_init = method_name == "<init>";
    let use_virtual_dispatch = !is_static && !is_private && !is_init;

    let iae_trace = std::env::var_os("RUSTJVM_IAE_TRACE").is_some();
    if iae_trace {
        eprintln!("[Method.invoke] about to invoke: class={} method={} desc={} is_static={} use_virtual={}",
                  class_name, method_name, descriptor, is_static, use_virtual_dispatch);
    }
    let result = if use_virtual_dispatch {
        // invoke_virtual takes the receiver separately and prepends it.
        // We already pushed receiver as invoke_args[0]; strip it for the
        // virtual call.
        let recv = match invoke_args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => {
                return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Method.invoke: null receiver for virtual dispatch".to_string()),
                }
                .into());
            }
        };
        let virtual_args: &[Value] = &invoke_args[1..];
        if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() {
            eprintln!("[Method.invoke] virtual class={} method={} desc={} recv={:p}",
                      class_name, method_name, descriptor, recv.as_ptr());
        }
        match ctx.invoke_virtual(recv, &method_name, &descriptor, virtual_args) {
            Ok(v) => v,
            Err(failure) => {
                return Err(wrap_as_invocation_target_exception(ctx, failure));
            }
        }
    } else {
        match ctx.invoke(&class_name, &method_name, &descriptor, &invoke_args) {
            Ok(v) => v,
            Err(failure) => {
                return Err(wrap_as_invocation_target_exception(ctx, failure));
            }
        }
    };

    // Box the return value
    match result {
        Some(val) => {
            let boxed = box_value(ctx, val, &ret_desc);
            Ok(Some(boxed))
        }
        None => Ok(Some(Value::Object(None))), // void method returns null
    }
}

// ---------------------------------------------------------------------------
// Class.getDeclaredMethods / getDeclaredMethod
// ---------------------------------------------------------------------------

/// F2 — synthetic-JDK reflection augmentation table.
///
/// When a JDK class is loaded as a synthetic stub (no real `.class` file
/// found on the classpath, see `class_manager::create_synthetic_stub`)
/// its `class.methods` is empty, so `declared_methods` returns an empty
/// list. Reflective lookups via `Class.getDeclaredMethod(...)` therefore
/// throw `NoSuchMethodException` even though the JDK contractually
/// declares those methods (and we usually back them with native
/// implementations registered in `lib.rs`).
///
/// This table provides the missing public/protected method signatures
/// for the well-known JDK classes that real-world frameworks reflect on.
/// CGLIB's `ReflectUtils.<clinit>` is the canonical caller — it does:
///
///   ClassLoader.class.getDeclaredMethod(
///       "defineClass",
///       String.class, byte[].class, int.class, int.class,
///       ProtectionDomain.class);
///
/// and previously NSME'd because our synthetic `java/lang/ClassLoader`
/// stub had no declared methods.
///
/// The augmentation is applied **only** when a method is missing from
/// `ctx.declared_methods(...)` — real-class loading (when the JDK
/// bytecode is present) takes precedence, exactly as the JDK contract
/// demands.
///
/// Each entry is `(name, descriptor, access_flags)` — `access_flags`
/// uses the JVM bit values (ACC_PUBLIC=0x1, ACC_PROTECTED=0x4,
/// ACC_FINAL=0x10, ACC_NATIVE=0x100, ACC_STATIC=0x8). The flags
/// reflect the OpenJDK 25 declarations.
fn synthetic_jdk_method_decls(class_name: &str) -> &'static [(&'static str, &'static str, u16)] {
    match class_name {
        // java.lang.ClassLoader — surface the public/protected `defineClass`
        // overloads + companion methods that frameworks reflect on. Access
        // flags match OpenJDK 25 `java/lang/ClassLoader.java`.
        //
        // ACC_PROTECTED|ACC_FINAL = 0x14
        // ACC_PUBLIC              = 0x01
        // ACC_PUBLIC|ACC_STATIC   = 0x09
        "java/lang/ClassLoader" => &[
            // protected final Class<?> defineClass(byte[] b, int off, int len)
            //   — DEPRECATED legacy overload (no name).
            ("defineClass", "([BII)Ljava/lang/Class;", 0x14),
            // protected final Class<?> defineClass(String name, byte[] b, int off, int len)
            ("defineClass", "(Ljava/lang/String;[BII)Ljava/lang/Class;", 0x14),
            // protected final Class<?> defineClass(String name, byte[] b, int off, int len,
            //                                      ProtectionDomain protectionDomain)
            //   — the overload CGLIB's ReflectUtils.<clinit> reflects on.
            (
                "defineClass",
                "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;",
                0x14,
            ),
            // protected final Class<?> defineClass(String name, ByteBuffer b,
            //                                      ProtectionDomain protectionDomain)
            (
                "defineClass",
                "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
                0x14,
            ),
            // protected final void resolveClass(Class<?> c)
            ("resolveClass", "(Ljava/lang/Class;)V", 0x14),
            // protected Class<?> findClass(String name)
            ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x04),
            // protected final Class<?> findLoadedClass(String name)
            ("findLoadedClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x14),
            // protected final Class<?> findSystemClass(String name)
            ("findSystemClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x14),
            // public Class<?> loadClass(String name)
            ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x01),
            // public final ClassLoader getParent()
            ("getParent", "()Ljava/lang/ClassLoader;", 0x11),
            // public String getName()
            ("getName", "()Ljava/lang/String;", 0x01),
            // public static ClassLoader getSystemClassLoader()
            (
                "getSystemClassLoader",
                "()Ljava/lang/ClassLoader;",
                0x09,
            ),
            // public static ClassLoader getPlatformClassLoader()
            (
                "getPlatformClassLoader",
                "()Ljava/lang/ClassLoader;",
                0x09,
            ),
            // protected static boolean registerAsParallelCapable()
            ("registerAsParallelCapable", "()Z", 0x0c),
            // public URL getResource(String name)
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;", 0x01),
            // public InputStream getResourceAsStream(String name)
            (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
                0x01,
            ),
        ],
        // G2: java/lang/reflect/Method — surface canonical declared
        // methods so that frameworks (ByteBuddy, Mockito) which call
        // `Method.class.getDeclaredMethods()` get a non-empty array.
        // ByteBuddy's `JavaDispatcher.<clinit>` iterates these and
        // would NPE if reflection on Method itself returned an empty
        // (or null) array.
        //
        // ACC_PUBLIC = 0x01; ACC_PUBLIC|ACC_NATIVE = 0x101.
        // All Method methods are public (final on a few — 0x11).
        "java/lang/reflect/Method" => &[
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getModifiers", "()I", 0x01),
            ("getReturnType", "()Ljava/lang/Class;", 0x01),
            ("getParameterTypes", "()[Ljava/lang/Class;", 0x01),
            ("getExceptionTypes", "()[Ljava/lang/Class;", 0x01),
            ("getParameterCount", "()I", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", 0x01),
            ("getGenericReturnType", "()Ljava/lang/reflect/Type;", 0x01),
            ("getGenericParameterTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getGenericExceptionTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getDefaultValue", "()Ljava/lang/Object;", 0x01),
            ("isVarArgs", "()Z", 0x01),
            ("isBridge", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("isDefault", "()Z", 0x01),
            ("invoke", "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;", 0x81),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("equals", "(Ljava/lang/Object;)Z", 0x01),
            ("hashCode", "()I", 0x01),
            ("toShortSignature", "()Ljava/lang/String;", 0x00),
        ],
        // G2: java/lang/reflect/Field — same rationale.
        "java/lang/reflect/Field" => &[
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getModifiers", "()I", 0x01),
            ("getType", "()Ljava/lang/Class;", 0x01),
            ("getGenericType", "()Ljava/lang/reflect/Type;", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("get", "(Ljava/lang/Object;)Ljava/lang/Object;", 0x01),
            ("set", "(Ljava/lang/Object;Ljava/lang/Object;)V", 0x01),
            ("getInt", "(Ljava/lang/Object;)I", 0x01),
            ("setInt", "(Ljava/lang/Object;I)V", 0x01),
            ("getLong", "(Ljava/lang/Object;)J", 0x01),
            ("setLong", "(Ljava/lang/Object;J)V", 0x01),
            ("getBoolean", "(Ljava/lang/Object;)Z", 0x01),
            ("setBoolean", "(Ljava/lang/Object;Z)V", 0x01),
            ("getByte", "(Ljava/lang/Object;)B", 0x01),
            ("setByte", "(Ljava/lang/Object;B)V", 0x01),
            ("getChar", "(Ljava/lang/Object;)C", 0x01),
            ("setChar", "(Ljava/lang/Object;C)V", 0x01),
            ("getShort", "(Ljava/lang/Object;)S", 0x01),
            ("setShort", "(Ljava/lang/Object;S)V", 0x01),
            ("getFloat", "(Ljava/lang/Object;)F", 0x01),
            ("setFloat", "(Ljava/lang/Object;F)V", 0x01),
            ("getDouble", "(Ljava/lang/Object;)D", 0x01),
            ("setDouble", "(Ljava/lang/Object;D)V", 0x01),
            ("isEnumConstant", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("equals", "(Ljava/lang/Object;)Z", 0x01),
            ("hashCode", "()I", 0x01),
        ],
        // G2: java/lang/reflect/Constructor — same rationale.
        "java/lang/reflect/Constructor" => &[
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getModifiers", "()I", 0x01),
            ("getParameterTypes", "()[Ljava/lang/Class;", 0x01),
            ("getExceptionTypes", "()[Ljava/lang/Class;", 0x01),
            ("getParameterCount", "()I", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", 0x01),
            ("getGenericParameterTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getGenericExceptionTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("newInstance", "([Ljava/lang/Object;)Ljava/lang/Object;", 0x81),
            ("isVarArgs", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("equals", "(Ljava/lang/Object;)Z", 0x01),
            ("hashCode", "()I", 0x01),
        ],
        // G2: java/lang/reflect/Executable (parent of Method + Constructor).
        // Frameworks sometimes reflect on the abstract parent class.
        "java/lang/reflect/Executable" => &[
            ("getName", "()Ljava/lang/String;", 0x401), // ACC_PUBLIC|ACC_ABSTRACT
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x401),
            ("getModifiers", "()I", 0x401),
            ("getParameterTypes", "()[Ljava/lang/Class;", 0x401),
            ("getExceptionTypes", "()[Ljava/lang/Class;", 0x401),
            ("getParameterCount", "()I", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x401),
            ("getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", 0x401),
            ("getGenericParameterTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getGenericExceptionTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getParameters", "()[Ljava/lang/reflect/Parameter;", 0x01),
            ("isVarArgs", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x401),
        ],
        // WP2.1-narrow — JDBC SPI interface methods. The native registry
        // already pins the canonical natives (see WP7.2 anchor tests
        // `each_jdbc_core_type_has_registered_natives` /
        // `each_jdbc_core_type_has_multiple_anchor_natives` in
        // `vm/tests/wp7_2_jdbc_core_types_reachable.rs`), but the synthetic
        // stub for these interfaces ships with `methods: vec![]`, so
        // `Class.getDeclaredMethods()` returned an empty array even though
        // the dispatch path itself worked. This synthetic-method table is
        // the documented hook (see [`synthetic_jdk_method_decls`] header)
        // for surfacing those declarations to the reflection layer when
        // real JDK bytecode is unavailable. All flags are
        // ACC_PUBLIC|ACC_ABSTRACT (0x401) — these are interface methods.
        //
        // The lists below are subsets of the JDBC SPI surface, chosen to
        // mirror the WP7.2 width-of-surface anchors so frameworks holding
        // real JDBC bytecode (HikariCP wrapping, ByteBuddy stubbing,
        // Spring/Hibernate proxies) see a non-empty reflective surface.
        "java/sql/Connection" => &[
            ("createStatement", "()Ljava/sql/Statement;", 0x401),
            (
                "prepareStatement",
                "(Ljava/lang/String;)Ljava/sql/PreparedStatement;",
                0x401,
            ),
            (
                "prepareCall",
                "(Ljava/lang/String;)Ljava/sql/CallableStatement;",
                0x401,
            ),
            ("getMetaData", "()Ljava/sql/DatabaseMetaData;", 0x401),
            ("close", "()V", 0x401),
            ("isClosed", "()Z", 0x401),
            ("setAutoCommit", "(Z)V", 0x401),
            ("getAutoCommit", "()Z", 0x401),
            ("commit", "()V", 0x401),
            ("rollback", "()V", 0x401),
            ("setReadOnly", "(Z)V", 0x401),
            ("isReadOnly", "()Z", 0x401),
            ("setCatalog", "(Ljava/lang/String;)V", 0x401),
            ("getCatalog", "()Ljava/lang/String;", 0x401),
            ("setSchema", "(Ljava/lang/String;)V", 0x401),
            ("getSchema", "()Ljava/lang/String;", 0x401),
            ("isValid", "(I)Z", 0x401),
        ],
        "java/sql/Statement" => &[
            ("execute", "(Ljava/lang/String;)Z", 0x401),
            (
                "executeQuery",
                "(Ljava/lang/String;)Ljava/sql/ResultSet;",
                0x401,
            ),
            ("executeUpdate", "(Ljava/lang/String;)I", 0x401),
            ("close", "()V", 0x401),
            ("isClosed", "()Z", 0x401),
            ("getConnection", "()Ljava/sql/Connection;", 0x401),
            ("getResultSet", "()Ljava/sql/ResultSet;", 0x401),
            ("getUpdateCount", "()I", 0x401),
            ("setQueryTimeout", "(I)V", 0x401),
            ("getQueryTimeout", "()I", 0x401),
            ("cancel", "()V", 0x401),
        ],
        "java/sql/PreparedStatement" => &[
            ("execute", "()Z", 0x401),
            ("executeQuery", "()Ljava/sql/ResultSet;", 0x401),
            ("executeUpdate", "()I", 0x401),
            ("setInt", "(II)V", 0x401),
            ("setLong", "(IJ)V", 0x401),
            ("setString", "(ILjava/lang/String;)V", 0x401),
            ("setBoolean", "(IZ)V", 0x401),
            ("setNull", "(II)V", 0x401),
            ("setObject", "(ILjava/lang/Object;)V", 0x401),
            ("clearParameters", "()V", 0x401),
            ("close", "()V", 0x401),
        ],
        "java/sql/ResultSet" => &[
            ("next", "()Z", 0x401),
            ("close", "()V", 0x401),
            ("wasNull", "()Z", 0x401),
            ("getString", "(I)Ljava/lang/String;", 0x401),
            ("getString", "(Ljava/lang/String;)Ljava/lang/String;", 0x401),
            ("getInt", "(I)I", 0x401),
            ("getInt", "(Ljava/lang/String;)I", 0x401),
            ("getLong", "(I)J", 0x401),
            ("getLong", "(Ljava/lang/String;)J", 0x401),
            ("getBoolean", "(I)Z", 0x401),
            ("getBoolean", "(Ljava/lang/String;)Z", 0x401),
            ("getObject", "(I)Ljava/lang/Object;", 0x401),
            ("getObject", "(Ljava/lang/String;)Ljava/lang/Object;", 0x401),
            ("getMetaData", "()Ljava/sql/ResultSetMetaData;", 0x401),
            ("isClosed", "()Z", 0x401),
        ],
        "java/sql/Driver" => &[
            (
                "connect",
                "(Ljava/lang/String;Ljava/util/Properties;)Ljava/sql/Connection;",
                0x401,
            ),
            ("acceptsURL", "(Ljava/lang/String;)Z", 0x401),
            ("getMajorVersion", "()I", 0x401),
            ("getMinorVersion", "()I", 0x401),
            ("jdbcCompliant", "()Z", 0x401),
        ],
        "java/sql/DatabaseMetaData" => &[
            (
                "getDatabaseProductName",
                "()Ljava/lang/String;",
                0x401,
            ),
            (
                "getDatabaseProductVersion",
                "()Ljava/lang/String;",
                0x401,
            ),
            ("getDriverName", "()Ljava/lang/String;", 0x401),
            ("getDriverVersion", "()Ljava/lang/String;", 0x401),
            ("getURL", "()Ljava/lang/String;", 0x401),
            ("getUserName", "()Ljava/lang/String;", 0x401),
            ("getDatabaseMajorVersion", "()I", 0x401),
            ("getDatabaseMinorVersion", "()I", 0x401),
            ("getJDBCMajorVersion", "()I", 0x401),
            ("getJDBCMinorVersion", "()I", 0x401),
            ("getConnection", "()Ljava/sql/Connection;", 0x401),
        ],
        // WP2.1-class-modern — surface the modern `java.lang.Class` API
        // methods (Java 11–25 sealed-class / record-class / nest-mate
        // accessors plus the canonical reflection-info methods) so
        // ByteBuddy's `TypeDescription.forLoadedType(Class.class)` and
        // Hibernate's record/sealed scanners see a non-empty
        // declared-method list when the synthetic-JDK `Class` stub is in
        // use. The natives backing each entry are already registered in
        // `lib.rs` / `phases_early.rs` / `lang_reflect.rs` — this table
        // surfaces them to the reflection layer.
        //
        // Flags: 0x01 ACC_PUBLIC, 0x11 ACC_PUBLIC|ACC_FINAL,
        //        0x101 ACC_PUBLIC|ACC_NATIVE.
        // `Class` itself is final, so all instance methods are effectively
        // final — but the JDK source marks only a few that way; we follow
        // the OpenJDK 25 declarations to stay byte-compatible.
        "java/lang/Class" => &[
            // Identity / naming
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getSimpleName", "()Ljava/lang/String;", 0x01),
            ("getCanonicalName", "()Ljava/lang/String;", 0x01),
            ("getTypeName", "()Ljava/lang/String;", 0x01),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("descriptorString", "()Ljava/lang/String;", 0x01),
            // Modifiers / shape predicates
            ("getModifiers", "()I", 0x01),
            ("isInterface", "()Z", 0x101),
            ("isArray", "()Z", 0x101),
            ("isPrimitive", "()Z", 0x101),
            ("isAnnotation", "()Z", 0x101),
            ("isSynthetic", "()Z", 0x01),
            ("isEnum", "()Z", 0x01),
            ("isRecord", "()Z", 0x01),
            ("isSealed", "()Z", 0x01),
            ("isHidden", "()Z", 0x101),
            ("isAnonymousClass", "()Z", 0x01),
            ("isLocalClass", "()Z", 0x01),
            ("isMemberClass", "()Z", 0x01),
            ("isInstance", "(Ljava/lang/Object;)Z", 0x101),
            ("isAssignableFrom", "(Ljava/lang/Class;)Z", 0x101),
            // Hierarchy
            ("getSuperclass", "()Ljava/lang/Class;", 0x101),
            ("getInterfaces", "()[Ljava/lang/Class;", 0x01),
            ("getGenericSuperclass", "()Ljava/lang/reflect/Type;", 0x01),
            ("getGenericInterfaces", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getComponentType", "()Ljava/lang/Class;", 0x01),
            ("getEnclosingClass", "()Ljava/lang/Class;", 0x01),
            ("getEnclosingMethod", "()Ljava/lang/reflect/Method;", 0x01),
            ("getEnclosingConstructor", "()Ljava/lang/reflect/Constructor;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getNestHost", "()Ljava/lang/Class;", 0x01),
            ("getNestMembers", "()[Ljava/lang/Class;", 0x01),
            ("isNestmateOf", "(Ljava/lang/Class;)Z", 0x01),
            // Sealed-class API (Java 17+)
            ("getPermittedSubclasses", "()[Ljava/lang/Class;", 0x01),
            // Record API (Java 16+)
            ("getRecordComponents", "()[Ljava/lang/reflect/RecordComponent;", 0x01),
            // Reflection — declared / inherited members
            ("getDeclaredFields", "()[Ljava/lang/reflect/Field;", 0x01),
            ("getDeclaredMethods", "()[Ljava/lang/reflect/Method;", 0x01),
            ("getDeclaredConstructors", "()[Ljava/lang/reflect/Constructor;", 0x01),
            ("getDeclaredClasses", "()[Ljava/lang/Class;", 0x01),
            (
                "getDeclaredField",
                "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
                0x01,
            ),
            (
                "getDeclaredMethod",
                "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                0x81,
            ),
            (
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                0x81,
            ),
            ("getFields", "()[Ljava/lang/reflect/Field;", 0x01),
            ("getMethods", "()[Ljava/lang/reflect/Method;", 0x01),
            ("getConstructors", "()[Ljava/lang/reflect/Constructor;", 0x01),
            ("getClasses", "()[Ljava/lang/Class;", 0x01),
            (
                "getField",
                "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
                0x01,
            ),
            (
                "getMethod",
                "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                0x81,
            ),
            (
                "getConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                0x81,
            ),
            // Annotations (AnnotatedElement surface + the annotated-type API)
            (
                "getAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            (
                "getDeclaredAnnotations",
                "()[Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "getAnnotationsByType",
                "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "getDeclaredAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "getDeclaredAnnotationsByType",
                "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "isAnnotationPresent",
                "(Ljava/lang/Class;)Z",
                0x01,
            ),
            (
                "getAnnotatedSuperclass",
                "()Ljava/lang/reflect/AnnotatedType;",
                0x01,
            ),
            (
                "getAnnotatedInterfaces",
                "()[Ljava/lang/reflect/AnnotatedType;",
                0x01,
            ),
            // Class loader / module / signers / protection domain
            ("getClassLoader", "()Ljava/lang/ClassLoader;", 0x01),
            ("getModule", "()Ljava/lang/Module;", 0x01),
            ("getPackage", "()Ljava/lang/Package;", 0x01),
            ("getPackageName", "()Ljava/lang/String;", 0x01),
            (
                "getProtectionDomain",
                "()Ljava/security/ProtectionDomain;",
                0x01,
            ),
            ("getSigners", "()[Ljava/lang/Object;", 0x01),
            ("getEnumConstants", "()[Ljava/lang/Object;", 0x01),
            (
                "getResource",
                "(Ljava/lang/String;)Ljava/net/URL;",
                0x01,
            ),
            (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
                0x01,
            ),
            // Generics
            ("getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;", 0x01),
            // Casts / forName
            (
                "cast",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                0x01,
            ),
            (
                "asSubclass",
                "(Ljava/lang/Class;)Ljava/lang/Class;",
                0x01,
            ),
            (
                "forName",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                0x09,
            ),
            (
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                0x09,
            ),
            (
                "newInstance",
                "()Ljava/lang/Object;",
                0x01,
            ),
            (
                "desiredAssertionStatus",
                "()Z",
                0x01,
            ),
            ("arrayType", "()Ljava/lang/Class;", 0x01),
            ("componentType", "()Ljava/lang/Class;", 0x01),
        ],
        _ => &[],
    }
}

/// F2 — build a `MethodMetadata` for a synthetic JDK declaration.
fn synthetic_method_meta(
    decl: &(&'static str, &'static str, u16),
    declaring_class_id: ClassId,
) -> MethodMetadata {
    MethodMetadata {
        name: decl.0.to_string(),
        descriptor: decl.1.to_string(),
        access_flags: decl.2,
        declaring_class_id,
        exceptions: Vec::new(),
    }
}

/// F2 — return the merged `declared_methods` for `class_id`, augmenting
/// with the synthetic JDK declarations from
/// [`synthetic_jdk_method_decls`] whenever the class file shipped no
/// matching method (typical of synthetic-stub bootstrap loading).
///
/// A method is considered "already present" if a (name, descriptor)
/// pair exists in `ctx.declared_methods`; the synthetic entry is then
/// skipped. This preserves real-class precedence — when JDK bytecode
/// is loaded, its declarations win.
fn declared_methods_with_synthetic(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Vec<MethodMetadata> {
    let mut methods = ctx.declared_methods(class_id);

    let class_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return methods,
    };

    let synth = synthetic_jdk_method_decls(&class_name);
    if synth.is_empty() {
        return methods;
    }

    for decl in synth {
        let already_present = methods
            .iter()
            .any(|m| m.name == decl.0 && m.descriptor == decl.1);
        if !already_present {
            methods.push(synthetic_method_meta(decl, class_id));
        }
    }

    methods
}

pub(crate) fn native_class_get_declared_methods(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredMethods on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // Primitive or array type
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let methods = declared_methods_with_synthetic(ctx, class_id);
    // Filter out <init> and <clinit>
    let visible: Vec<&MethodMetadata> = methods
        .iter()
        .filter(|m| m.name != "<init>" && m.name != "<clinit>")
        .collect();

    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), visible.len());
    for (i, meta) in visible.iter().enumerate() {
        let method_obj = create_method_object(ctx, meta);
        ctx.set_array_element(arr, i, Value::Object(Some(method_obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_declared_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredMethod on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredMethod: name is null".to_string()),
            }
            .into())
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();

    // Parameter types array (args[2]) — may be null or empty
    let param_types_arr = match args.get(2) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
                message: target_name,
            }
            .into())
        }
    };

    // F2: include synthetic JDK declarations for classes loaded as
    // synthetic stubs (e.g. java/lang/ClassLoader has no real .class
    // file in synthetic-jdk mode, so its `methods` list is empty).
    let methods = declared_methods_with_synthetic(ctx, class_id);

    // If param_types is provided, match by name + parameter count/types
    for meta in &methods {
        if meta.name != target_name || meta.name == "<init>" || meta.name == "<clinit>" {
            continue;
        }

        if let Some(pt_arr) = param_types_arr {
            // Match parameter types
            let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
            let expected_count = ctx.array_length(pt_arr);
            if param_descs.len() != expected_count {
                continue;
            }
            // Check each parameter type matches
            let mut matched = true;
            for (i, pdesc) in param_descs.iter().enumerate() {
                let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        matched = false;
                        break;
                    }
                };
                let expected_name = mirror_class_name(ctx, expected_mirror).unwrap_or_default();
                let actual_mirror = descriptor_to_class_mirror(ctx, pdesc);
                let actual_name = mirror_class_name(ctx, actual_mirror).unwrap_or_default();
                if expected_name != actual_name {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
        }

        let method_obj = create_method_object(ctx, meta);
        return Ok(Some(Value::Object(Some(method_obj))));
    }

    Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
        message: target_name,
    }
    .into())
}

// ---------------------------------------------------------------------------
// java.lang.reflect.Constructor — object layout and natives
// ---------------------------------------------------------------------------

/// Number of "extra" slots appended after the JDK Constructor layout to
/// hold RustJVM-specific metadata (not present on real JDK Constructor):
///   +0 → String (raw descriptor, e.g. "(I)V")
///   +1 → Int    (parameter count — cached)
///   +2 → Int    (accessible flag, 0 or 1)
const CONSTRUCTOR_EXTRA_SLOTS: usize = 3;
const CONSTRUCTOR_EXTRA_OFFSET_DESC: usize = 0;
const CONSTRUCTOR_EXTRA_OFFSET_PARAM_COUNT: usize = 1;
const CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE: usize = 2;

/// Legacy synthetic Constructor width — kept as a floor so the object
/// is always large enough for the synthetic writes and so tests using
/// the MockNativeContext (which returns 0 for `class_num_total_fields`)
/// still have room.
const CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR: usize = 6;

#[cfg(test)]
const CONSTRUCTOR_NUM_FIELDS: usize = CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR;

fn constructor_extra_base(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    core::cmp::max(
        CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR,
        ctx.class_num_total_fields(class_id),
    )
}

/// C6 fix (mirrors `create_method_object`): populate real JDK-named
/// fields via `set_field_by_name` so Java bytecode `Getfield clazz` /
/// `Getfield modifiers` reads land on the correct inherited slots. Store
/// RustJVM-specific metadata in extra slots appended after the JDK
/// layout.
pub(crate) fn create_constructor_object(
    ctx: &mut dyn NativeContext,
    meta: &MethodMetadata,
) -> rustjvm_types::ObjectRef {
    let class_id = ctx.ensure_class_initialized("java/lang/reflect/Constructor")
        .unwrap_or(ClassId::new(0));

    let jdk_layout_fields = ctx.class_num_total_fields(class_id);
    let base = core::cmp::max(CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR, jdk_layout_fields);
    let num_fields = base + CONSTRUCTOR_EXTRA_SLOTS;
    let obj = ctx.alloc_object(class_id, num_fields);

    let class_mirror = ctx.get_class_mirror(meta.declaring_class_id);

    // Parse descriptor for param types
    let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
    let param_arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), param_descs.len());
    for (i, pdesc) in param_descs.iter().enumerate() {
        let pmirror = descriptor_to_class_mirror(ctx, pdesc);
        ctx.set_array_element(param_arr, i, Value::Object(Some(pmirror)));
    }
    let desc_str = ctx.create_string(&meta.descriptor);

    // WP2.1 — populate `exceptionTypes` from the JVMS §4.7.5 `Exceptions`
    // attribute. The JDK `Constructor.getExceptionTypes()` Java method
    // does `return exceptionTypes.clone();`, which NPEs if null. CGLib's
    // Enhancer.emitConstructors calls this on every superclass constructor
    // during proxy class generation — see ReflectUtils.getExceptionTypes
    // (ReflectUtils.java:133/605).
    let exception_names = ctx.method_exceptions(
        meta.declaring_class_id,
        &meta.name,
        &meta.descriptor,
    );
    let exception_arr = ctx.new_ref_array(
        rustjvm_types::ClassId::new(0),
        exception_names.len(),
    );
    for (i, name) in exception_names.iter().enumerate() {
        let desc = format!("L{name};");
        let mirror = descriptor_to_class_mirror(ctx, &desc);
        ctx.set_array_element(exception_arr, i, Value::Object(Some(mirror)));
    }

    // --- Real JDK Constructor layout ---
    ctx.set_field_by_name(obj, "clazz", Value::Object(Some(class_mirror)));
    ctx.set_field_by_name(obj, "parameterTypes", Value::Object(Some(param_arr)));
    ctx.set_field_by_name(obj, "exceptionTypes", Value::Object(Some(exception_arr)));
    ctx.set_field_by_name(obj, "modifiers", Value::Int(meta.access_flags as i32));
    ctx.set_field_by_name(obj, "slot", Value::Int(0));

    // --- RustJVM extra metadata ---
    ctx.set_field(
        obj,
        base + CONSTRUCTOR_EXTRA_OFFSET_DESC,
        Value::Object(Some(desc_str)),
    );
    ctx.set_field(
        obj,
        base + CONSTRUCTOR_EXTRA_OFFSET_PARAM_COUNT,
        Value::Int(param_descs.len() as i32),
    );
    ctx.set_field(
        obj,
        base + CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(0),
    );

    obj
}

pub(crate) fn read_constructor_descriptor(
    ctx: &dyn NativeContext,
    ctor_obj: rustjvm_types::ObjectRef,
) -> Option<String> {
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    match ctx.get_field(ctor_obj, base + CONSTRUCTOR_EXTRA_OFFSET_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn read_constructor_param_count(
    ctx: &dyn NativeContext,
    ctor_obj: rustjvm_types::ObjectRef,
) -> i32 {
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    match ctx.get_field(ctor_obj, base + CONSTRUCTOR_EXTRA_OFFSET_PARAM_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn read_constructor_accessible(
    ctx: &dyn NativeContext,
    ctor_obj: rustjvm_types::ObjectRef,
) -> bool {
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    match ctx.get_field(ctor_obj, base + CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

pub(crate) fn write_constructor_accessible(
    ctx: &mut dyn NativeContext,
    ctor_obj: rustjvm_types::ObjectRef,
    value: bool,
) {
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    ctx.set_field(
        ctor_obj,
        base + CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(if value { 1 } else { 0 }),
    );
}

/// Public wrapper used by `lang_reflect::native_constructor_try_set_accessible`.
pub(crate) fn write_constructor_accessible_external(
    ctx: &mut dyn NativeContext,
    ctor_obj: rustjvm_types::ObjectRef,
    value: bool,
) {
    write_constructor_accessible(ctx, ctor_obj, value);
}

// --- Constructor getters ---

pub(crate) fn native_constructor_get_parameter_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "parameterTypes")))
}

pub(crate) fn native_constructor_get_modifiers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field_by_name(this, "modifiers")))
}

pub(crate) fn native_constructor_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "clazz")))
}

pub(crate) fn native_constructor_get_parameter_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(read_constructor_param_count(ctx, this))))
}

// --- Constructor.newInstance ---

pub(crate) fn native_constructor_new_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Constructor.newInstance: null Constructor".to_string()),
            }
            .into())
        }
    };

    // Get declaring class name (C6: real-JDK field name)
    let declaring_mirror = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(m)) => m,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Constructor.newInstance: no declaring class".to_string()),
            }
            .into())
        }
    };
    let class_name = match mirror_class_name(ctx, declaring_mirror) {
        Some(n) if !n.is_empty() => n,
        _ => match ctx.class_id_from_mirror(declaring_mirror) {
            Some(cid) => ctx.class_name_of_id(cid).unwrap_or_default(),
            None => String::new(),
        },
    };
    if class_name.is_empty() {
        return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
            message: "Constructor.newInstance: declaring class has empty name".to_string(),
        }
        .into());
    }

    // NEW-19: JPMS deep-reflection check. Accessible flag lives in a
    // RustJVM extra slot; when true the check is already paid (JEP 403).
    let accessible = read_constructor_accessible(ctx, this);
    if let Err(msg) = check_reflection_module_access(ctx, &class_name, accessible) {
        return Err(rustjvm_types::error::RuntimeError::IllegalAccessException {
            message: format!("Constructor.newInstance: {class_name}: {msg}"),
        }
        .into());
    }

    // Descriptor is stashed in the RustJVM extra slot (C6).
    let descriptor = read_constructor_descriptor(ctx, this).unwrap_or_else(|| "()V".to_string());

    // Reject abstract classes / interfaces before allocating — matches
    // `java.lang.reflect.Constructor.newInstance` which throws
    // InstantiationException on abstract targets.
    if let Some(cid) = ctx.class_id_by_name(&class_name) {
        let flags = ctx.class_access_flags(cid);
        let abstract_bit = rustjvm_types::access_flags::ACC_ABSTRACT;
        let iface_bit = rustjvm_types::access_flags::ACC_INTERFACE;
        if flags & (abstract_bit | iface_bit) != 0 {
            return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                message: format!(
                    "InstantiationException: cannot instantiate abstract/interface type {class_name}"
                ),
            }
            .into());
        }
    }

    // Allocate the new object
    let obj_val = ctx.new_object(&class_name)?;
    let obj = match obj_val {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                message: format!("Constructor.newInstance: failed to allocate {class_name}"),
            }
            .into())
        }
    };

    // Parse parameter types
    let (param_descs, _) = parse_descriptor_param_and_return(&descriptor);

    // Extract arguments from Object[] (args[1])
    let args_array = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };
    let actual_arg_count = match args_array {
        Some(arr) => ctx.array_length(arr),
        None => 0,
    };

    if actual_arg_count != param_descs.len() {
        return Err(illegal_arg_exc(format!(
            "Constructor.newInstance: wrong number of arguments for {class_name}: \
             expected {}, got {}",
            param_descs.len(),
            actual_arg_count,
        )));
    }

    // Build invocation args: [this_obj, ...coerced_args]
    let mut invoke_args: Vec<Value> = vec![Value::Object(Some(obj))];
    for (i, pdesc) in param_descs.iter().enumerate() {
        let arg_val = if let Some(arr) = args_array {
            ctx.get_array_element(arr, i)
        } else {
            Value::Object(None)
        };
        let coerced = coerce_arg_strict(ctx, arg_val, pdesc, "Constructor.newInstance argument")?;
        invoke_args.push(coerced);
    }

    // Call <init>; wrap any Java exception in InvocationTargetException per
    // `Constructor.newInstance` javadoc.
    match ctx.invoke(&class_name, "<init>", &descriptor, &invoke_args) {
        Ok(_) => Ok(Some(Value::Object(Some(obj)))),
        Err(failure) => Err(wrap_as_invocation_target_exception(ctx, failure)),
    }
}

// ---------------------------------------------------------------------------
// Class.getDeclaredConstructors / getDeclaredConstructor
// ---------------------------------------------------------------------------

pub(crate) fn native_class_get_declared_constructors(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredConstructors on null".to_string()),
            }
            .into())
        }
    };
    // JDK `Class.getDeclaredConstructors0(boolean publicOnly)` — when true,
    // returns only ACC_PUBLIC <init> (used by `Class.getConstructors()`).
    // When this native is invoked from the synthetic `getDeclaredConstructors()`
    // wrapper (args.len()==1), publicOnly is false.
    let public_only = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let methods = ctx.declared_methods(class_id);
    let constructors: Vec<&MethodMetadata> = methods
        .iter()
        .filter(|m| {
            m.name == "<init>"
                && (!public_only || (m.access_flags & 0x0001) != 0)
        })
        .collect();

    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), constructors.len());
    for (i, meta) in constructors.iter().enumerate() {
        let ctor_obj = create_constructor_object(ctx, meta);
        ctx.set_array_element(arr, i, Value::Object(Some(ctor_obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_declared_constructor(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredConstructor on null".to_string()),
            }
            .into())
        }
    };

    // Parameter types array (args[1])
    let param_types_arr = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
                message: "<init>".to_string(),
            }
            .into())
        }
    };

    let methods = ctx.declared_methods(class_id);

    for meta in &methods {
        if meta.name != "<init>" {
            continue;
        }

        let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);

        if let Some(pt_arr) = param_types_arr {
            let expected_count = ctx.array_length(pt_arr);
            if param_descs.len() != expected_count {
                continue;
            }
            let mut matched = true;
            for (i, pdesc) in param_descs.iter().enumerate() {
                let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        matched = false;
                        break;
                    }
                };
                let expected_name = mirror_class_name(ctx, expected_mirror).unwrap_or_default();
                let actual_mirror = descriptor_to_class_mirror(ctx, pdesc);
                let actual_name = mirror_class_name(ctx, actual_mirror).unwrap_or_default();
                if expected_name != actual_name {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
        } else {
            // No param types specified — match no-arg constructor
            if !param_descs.is_empty() {
                continue;
            }
        }

        let ctor_obj = create_constructor_object(ctx, meta);
        return Ok(Some(Value::Object(Some(ctor_obj))));
    }

    Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
        message: "<init>".to_string(),
    }
    .into())
}

// ---------------------------------------------------------------------------
// Inherited enumeration — getFields/getMethods/getConstructors
// ---------------------------------------------------------------------------

/// Collect all public fields from the class hierarchy (this class +
/// superclasses + interfaces). Mirrors `Class.privateGetPublicFields()`.
///
/// Per JDK semantics: for an interface, the superclass walk is skipped
/// (matches `collect_public_methods`).
fn collect_public_fields(
    ctx: &mut dyn NativeContext,
    class_id: rustjvm_types::ClassId,
) -> Vec<rustjvm_types::ObjectRef> {
    let mut result = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];

    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        let fields = ctx.declared_fields(cid);
        for meta in &fields {
            if (meta.access_flags & 0x0001) != 0 {
                // PUBLIC
                result.push(create_field_object(ctx, meta));
            }
        }
        // Walk superclass — skipped for interfaces (see
        // `collect_public_methods` rationale).
        if !ctx.is_interface_class(cid) {
            if let Some(parent) = ctx.superclass_of(cid) {
                stack.push(parent);
            }
        }
        // Walk interfaces
        for iface_id in ctx.class_interfaces(cid) {
            stack.push(iface_id);
        }
    }
    result
}

/// Collect all public methods from the class hierarchy.
///
/// Per JDK 25 `Class.privateGetPublicMethods()` semantics:
/// * For a **class**: walk declared public methods + super-interfaces +
///   superclass chain.
/// * For an **interface**: walk declared public methods + super-interfaces
///   only — **NEVER** the superclass (which is `java/lang/Object` per
///   JVMS but is intentionally skipped — see `Class.java`:
///   `Class<?> sc = isInterface() ? null : getSuperclass();`).
///
/// G2-fix (NegativeArraySizeException): walking the superclass chain for
/// an interface pulled in `java/lang/Object`'s public 0-arg methods
/// (`toString`, `hashCode`, `getClass`, `notify`, `notifyAll`). ByteBuddy's
/// `JavaDispatcher$DynamicClassLoader.invoker()` then calls
/// `parameterTypes.length - 1` and `anewarray` on the result —
/// underflowing to `-1` and triggering `NegativeArraySizeException`.
/// Real JDK 25 returns only the 2 declared `Invoker` methods; this fix
/// matches.
fn collect_public_methods(
    ctx: &mut dyn NativeContext,
    class_id: rustjvm_types::ClassId,
) -> Vec<rustjvm_types::ObjectRef> {
    let mut result = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];

    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        // F2: include synthetic JDK declarations so frameworks that
        // walk the public method table on a synthetic-stub class (e.g.
        // java/lang/ClassLoader) still see the JDK-contracted methods.
        let methods = declared_methods_with_synthetic(ctx, cid);
        for meta in &methods {
            if meta.name == "<init>" || meta.name == "<clinit>" {
                continue;
            }
            if (meta.access_flags & 0x0001) != 0 {
                // PUBLIC
                result.push(create_method_object(ctx, meta));
            }
        }
        // G2-fix: only walk the superclass chain for non-interface classes.
        // Interfaces (and their super-interfaces) intentionally skip the
        // superclass (which is java/lang/Object) per JDK semantics.
        if !ctx.is_interface_class(cid) {
            if let Some(parent) = ctx.superclass_of(cid) {
                stack.push(parent);
            }
        }
        for iface_id in ctx.class_interfaces(cid) {
            stack.push(iface_id);
        }
    }
    result
}

pub(crate) fn native_class_get_fields(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getFields on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let field_objs = collect_public_fields(ctx, class_id);
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), field_objs.len());
    for (i, fobj) in field_objs.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*fobj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_field(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getField on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getField: name is null".to_string()),
            }
            .into())
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NoSuchFieldException {
                field_name: target_name,
            }
            .into())
        }
    };

    // Walk class hierarchy for public field
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];
    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        let fields = ctx.declared_fields(cid);
        for meta in &fields {
            if meta.name == target_name && (meta.access_flags & 0x0001) != 0 {
                let field_obj = create_field_object(ctx, meta);
                return Ok(Some(Value::Object(Some(field_obj))));
            }
        }
        if let Some(parent) = ctx.superclass_of(cid) {
            stack.push(parent);
        }
        for iface_id in ctx.class_interfaces(cid) {
            stack.push(iface_id);
        }
    }

    Err(rustjvm_types::error::RuntimeError::NoSuchFieldException {
        field_name: target_name,
    }
    .into())
}

pub(crate) fn native_class_get_methods(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getMethods on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let method_objs = collect_public_methods(ctx, class_id);
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), method_objs.len());
    for (i, mobj) in method_objs.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*mobj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getMethod on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getMethod: name is null".to_string()),
            }
            .into())
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();
    let param_types_arr = match args.get(2) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
                message: target_name,
            }
            .into())
        }
    };

    // Walk class hierarchy for public method
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];
    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        // F2: include synthetic JDK declarations on each rung of the hierarchy
        // walk so a public method declared on a synthetic-stub superclass
        // (e.g. java/lang/ClassLoader) is still reachable from getMethod.
        let methods = declared_methods_with_synthetic(ctx, cid);
        for meta in &methods {
            if meta.name != target_name
                || meta.name == "<init>"
                || meta.name == "<clinit>"
                || (meta.access_flags & 0x0001) == 0
            {
                continue;
            }
            // Check parameter types if specified
            if let Some(pt_arr) = param_types_arr {
                let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
                let expected_count = ctx.array_length(pt_arr);
                if param_descs.len() != expected_count {
                    continue;
                }
                let mut matched = true;
                for (i, pdesc) in param_descs.iter().enumerate() {
                    let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                        Value::Object(Some(m)) => m,
                        _ => {
                            matched = false;
                            break;
                        }
                    };
                    let expected_name = mirror_class_name(ctx, expected_mirror).unwrap_or_default();
                    let actual_mirror = descriptor_to_class_mirror(ctx, pdesc);
                    let actual_name = mirror_class_name(ctx, actual_mirror).unwrap_or_default();
                    if expected_name != actual_name {
                        matched = false;
                        break;
                    }
                }
                if !matched {
                    continue;
                }
            }
            let method_obj = create_method_object(ctx, meta);
            return Ok(Some(Value::Object(Some(method_obj))));
        }
        if let Some(parent) = ctx.superclass_of(cid) {
            stack.push(parent);
        }
        for iface_id in ctx.class_interfaces(cid) {
            stack.push(iface_id);
        }
    }

    Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
        message: target_name,
    }
    .into())
}

pub(crate) fn native_class_get_constructors(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getConstructors on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let methods = ctx.declared_methods(class_id);
    let public_ctors: Vec<&MethodMetadata> = methods
        .iter()
        .filter(|m| m.name == "<init>" && (m.access_flags & 0x0001) != 0)
        .collect();

    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), public_ctors.len());
    for (i, meta) in public_ctors.iter().enumerate() {
        let ctor_obj = create_constructor_object(ctx, meta);
        ctx.set_array_element(arr, i, Value::Object(Some(ctor_obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getConstructor on null".to_string()),
            }
            .into())
        }
    };
    let param_types_arr = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
                message: "<init>".to_string(),
            }
            .into())
        }
    };

    let methods = ctx.declared_methods(class_id);
    for meta in &methods {
        if meta.name != "<init>" || (meta.access_flags & 0x0001) == 0 {
            continue;
        }
        let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);

        if let Some(pt_arr) = param_types_arr {
            let expected_count = ctx.array_length(pt_arr);
            if param_descs.len() != expected_count {
                continue;
            }
            let mut matched = true;
            for (i, pdesc) in param_descs.iter().enumerate() {
                let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        matched = false;
                        break;
                    }
                };
                let expected_name = mirror_class_name(ctx, expected_mirror).unwrap_or_default();
                let actual_mirror = descriptor_to_class_mirror(ctx, pdesc);
                let actual_name = mirror_class_name(ctx, actual_mirror).unwrap_or_default();
                if expected_name != actual_name {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
        } else if !param_descs.is_empty() {
            continue;
        }

        let ctor_obj = create_constructor_object(ctx, meta);
        return Ok(Some(Value::Object(Some(ctor_obj))));
    }

    Err(rustjvm_types::error::RuntimeError::NoSuchMethodException {
        message: "<init>".to_string(),
    }
    .into())
}

// --- Class.getInterfaces / Class.getModifiers ---

pub(crate) fn native_class_get_interfaces(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getInterfaces on null".to_string()),
            }
            .into())
        }
    };

    // WP2.5: if this is the synthetic `Proxy$Instance` class mirror,
    // return the most recently created proxy's interfaces array
    // (stored by `native_proxy_new_instance`). All proxies currently
    // share this class id, so we can't return per-instance interfaces
    // off the class itself — see the doc-comment in
    // `vm::runtime::proxy` for the last-wins limitation and the
    // Strategy-A path forward.
    if let Value::Object(Some(name_ref)) = ctx.get_field(this, 1) {
        if let Some(name) = ctx.read_string(name_ref) {
            if name == "java/lang/reflect/Proxy$Instance" {
                let bits = crate::proxy_last_interfaces_bits();
                if bits != 0 {
                    // SAFETY: the bits encode the heap pointer of an
                    // interfaces array allocated by
                    // `native_proxy_new_instance` and stored on the
                    // proxy at field 1. The proxy retains the array,
                    // so it's still live.
                    let arr_ref = unsafe {
                        rustjvm_types::ObjectRef::from_raw(bits as *mut u8)
                    };
                    return Ok(Some(Value::Object(Some(arr_ref))));
                }
                // No proxy has been created yet — fall through and
                // return an empty array.
                let empty = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
                return Ok(Some(Value::Object(Some(empty))));
            }
        }
    }

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let iface_ids = ctx.class_interfaces(class_id);
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), iface_ids.len());
    for (i, iface_id) in iface_ids.iter().enumerate() {
        let mirror = ctx.get_class_mirror(*iface_id);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_class_get_modifiers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };

    // JVMS §4.7.6: for nested classes, Class.getModifiers() returns the
    // `inner_class_access_flags` from the InnerClasses attribute entry whose
    // `inner_class_info_index` points at this class — NOT the class's own
    // `access_flags`. The class's own access_flags does not carry ACC_STATIC
    // (our `ClassAccessFlags` bitflag set does not even contain a STATIC
    // variant, since STATIC is not legal in the top-level ClassFile flags),
    // whereas the InnerClasses entry does.  This matters for reflection code
    // such as Jackson's check `!Modifier.isStatic(cls.getModifiers())` on a
    // `public static class` nested inside another class.
    //
    // The canonical location for the InnerClasses entry is the class itself
    // (javac emits an InnerClasses attribute on every class that references
    // a nested class, including the nested class's own class file referring
    // to itself).  We probe the class's inner_classes list for an entry
    // whose `inner_class` name equals this class's name; if found we return
    // those flags, otherwise we fall back to the class's own access_flags.
    let own_flags = ctx.class_access_flags(class_id);
    let effective_flags = if let Some(this_name) = ctx.class_name_of_id(class_id) {
        ctx.inner_classes(class_id)
            .into_iter()
            .find(|(inner, _outer, _name, _flags)| *inner == this_name)
            .map(|(_inner, _outer, _name, flags)| flags)
            .unwrap_or(own_flags)
    } else {
        own_flags
    };
    Ok(Some(Value::Int(effective_flags as i32)))
}

// ---------------------------------------------------------------------------
// Annotation support (Phase 20)
// ---------------------------------------------------------------------------

/// Annotation proxy: 4-field synthetic object
///   field 0 = String (annotation type descriptor, e.g. "Ljava/lang/Override;")
///   field 1 = Class mirror (annotation type class, or null)
///   field 2 = String[] (element names)
///   field 3 = Object[] (element values — boxed primitives, Strings, Class mirrors, etc.)
pub const ANN_PROXY_FIELDS: usize = 4;
pub const ANN_PROXY_TYPE_DESC: usize = 0;
pub const ANN_PROXY_TYPE_MIRROR: usize = 1;
pub const ANN_PROXY_ELEM_NAMES: usize = 2;
pub const ANN_PROXY_ELEM_VALUES: usize = 3;

/// Convert an annotation type descriptor to an internal class name.
/// E.g. "Ljava/lang/Override;" -> "java/lang/Override"
fn annotation_desc_to_class_name(desc: &str) -> Option<&str> {
    if desc.starts_with('L') && desc.ends_with(';') {
        Some(&desc[1..desc.len() - 1])
    } else {
        None
    }
}

/// Create an annotation proxy object from annotation data.
/// Fills in default values for elements not explicitly provided.
fn create_annotation_proxy(
    ctx: &mut dyn NativeContext,
    ann: &rustjvm_native_api::AnnotationData,
) -> ObjectRef {
    let proxy = alloc_concurrent_synthetic(ctx, "java/lang/annotation/AnnotationProxy", ANN_PROXY_FIELDS);
    let desc_str = ctx.create_string(&ann.type_descriptor);
    ctx.set_field(proxy, ANN_PROXY_TYPE_DESC, Value::Object(Some(desc_str)));

    let mut ann_class_id_opt = None;
    // Try to get the Class mirror for the annotation type. Load the annotation
    // class on demand if it hasn't been loaded yet — JUnit4's TestClass scanner
    // calls Annotation.annotationType() expecting a non-null Class (C44). If
    // this field is null, BlockJUnit4ClassRunner reports a dummy failure
    // because runsTopToBottom(Class) NPEs on equals().
    if let Some(class_name) = annotation_desc_to_class_name(&ann.type_descriptor) {
        let cid_opt = ctx.class_id_by_name(class_name).or_else(|| {
            // load_class returns the mirror; we only need the ClassId, so just
            // trigger the load and re-query by name.
            let _ = ctx.load_class(class_name);
            ctx.class_id_by_name(class_name)
        });
        if let Some(cid) = cid_opt {
            ann_class_id_opt = Some(cid);
            let mirror = ctx.get_class_mirror(cid);
            ctx.set_field(proxy, ANN_PROXY_TYPE_MIRROR, Value::Object(Some(mirror)));
        } else if std::env::var("RUSTJVM_IAE_TRACE").is_ok() {
            eprintln!("ANN-PROXY-NULL-MIRROR: annotation={} type_descriptor={} class_name={class_name} — type mirror NOT set (class load failed)",
                ann.type_descriptor, ann.type_descriptor);
        }
    } else if std::env::var("RUSTJVM_IAE_TRACE").is_ok() {
        eprintln!("ANN-PROXY-NULL-MIRROR: type_descriptor={} — annotation_desc_to_class_name returned None",
            ann.type_descriptor);
    }

    // Collect explicit elements with their declared return-type descriptor
    // (read from the annotation interface's abstract method, when available).
    // The descriptor is used as a fallback hint when the element value is an
    // EMPTY array — we'd otherwise pick `java/lang/Object` as the component
    // class, which trips Spring's `AnnotationUtils.adaptValue` into treating
    // the array as `Annotation[]` (under the lenient `Object` fallback in
    // `array_is_assignable`) and converts the empty `Object[]` into an empty
    // `AnnotationAttributes[]`.  That collapse later surfaces as
    // `IllegalArgumentException` in `AnnotationAttributes.assertAttributeType`
    // on `getStringArray("pattern")` for `@ComponentScan.Filter` (S111r19).
    let mut all_elements: Vec<(
        String,
        rustjvm_native_api::AnnotationElementValue,
        Option<String>,
    )> = ann
        .elements
        .iter()
        .map(|(n, v)| (n.clone(), v.clone(), None))
        .collect();

    // Fill in AnnotationDefault values for missing elements
    if let Some(ann_cid) = ann_class_id_opt {
        let methods = ctx.declared_methods(ann_cid);
        let explicit_names: std::collections::HashSet<String> =
            all_elements.iter().map(|(n, _, _)| n.clone()).collect();
        // For explicit elements, also backfill the return-type descriptor
        // so empty arrays carry the right component hint.
        for (name, _, desc_slot) in all_elements.iter_mut() {
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
            if let Some(default_val) = ctx.method_annotation_default(ann_cid, &m.name, &m.descriptor) {
                let ret_desc = m.descriptor.strip_prefix("()").map(|s| s.to_string());
                all_elements.push((m.name.clone(), default_val, ret_desc));
            }
        }
    }

    // Store element name→value pairs as parallel arrays
    let n = all_elements.len();
    let names_arr = ctx.new_ref_array(ClassId::new(0), n);
    let values_arr = ctx.new_ref_array(ClassId::new(0), n);
    for (i, (name, val, ret_desc)) in all_elements.iter().enumerate() {
        let name_str = ctx.create_string(name);
        ctx.set_array_element(names_arr, i, Value::Object(Some(name_str)));
        let java_val =
            annotation_element_to_java_typed(ctx, val, ret_desc.as_deref());
        ctx.set_array_element(values_arr, i, java_val);
    }
    ctx.set_field(proxy, ANN_PROXY_ELEM_NAMES, Value::Object(Some(names_arr)));
    ctx.set_field(proxy, ANN_PROXY_ELEM_VALUES, Value::Object(Some(values_arr)));
    proxy
}

/// Convert an AnnotationElementValue to a Java Value.
///
/// WP2.7: primitives are boxed into the **correct wrapper class** (not
/// `java.lang.Object`) so spec-compliant `Annotation.equals` can identify
/// them as wrappers and compare by underlying value, and so the proxy's
/// element accessor methods return wrappers that pass downstream
/// `instanceof Integer` checks.
pub(crate) fn annotation_element_to_java(
    ctx: &mut dyn NativeContext,
    val: &rustjvm_native_api::AnnotationElementValue,
) -> Value {
    annotation_element_to_java_typed(ctx, val, None)
}

/// S111r19 — typed variant: when called for a known annotation-element method,
/// the caller passes the method's return-type descriptor (e.g.
/// `[Ljava/lang/String;`).  Used to recover the array component class for
/// **empty** array values, which would otherwise default to
/// `java/lang/Object` and trip Spring's `AnnotationUtils.adaptValue` into
/// converting the empty `Object[]` to an empty `AnnotationAttributes[]`
/// (because our `array_is_assignable` lenient `Object` fallback green-lights
/// `Annotation[].isInstance(Object[])`).
pub(crate) fn annotation_element_to_java_typed(
    ctx: &mut dyn NativeContext,
    val: &rustjvm_native_api::AnnotationElementValue,
    return_type_desc: Option<&str>,
) -> Value {
    use rustjvm_native_api::AnnotationElementValue;
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
                Some("Z") => (
                    "java/lang/Boolean",
                    Value::Int(if *v != 0 { 1 } else { 0 }),
                ),
                Some("B") => ("java/lang/Byte", Value::Int(*v as i8 as i32)),
                Some("C") => ("java/lang/Character", Value::Int(*v & 0xFFFF)),
                Some("S") => ("java/lang/Short", Value::Int(*v as i16 as i32)),
                _ => ("java/lang/Integer", Value::Int(*v)),
            };
            let obj = crate::alloc_concurrent_synthetic(ctx, wrapper, 1);
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        AnnotationElementValue::Long(v) => {
            let obj = crate::alloc_concurrent_synthetic(ctx, "java/lang/Long", 1);
            ctx.set_field(obj, 0, Value::Long(*v));
            Value::Object(Some(obj))
        }
        AnnotationElementValue::Float(v) => {
            let obj = crate::alloc_concurrent_synthetic(ctx, "java/lang/Float", 1);
            ctx.set_field(obj, 0, Value::Float(*v));
            Value::Object(Some(obj))
        }
        AnnotationElementValue::Double(v) => {
            let obj = crate::alloc_concurrent_synthetic(ctx, "java/lang/Double", 1);
            ctx.set_field(obj, 0, Value::Double(*v));
            Value::Object(Some(obj))
        }
        AnnotationElementValue::StringVal(s) => {
            let str_obj = ctx.create_string(s);
            Value::Object(Some(str_obj))
        }
        AnnotationElementValue::Enum(type_desc, const_name) => {
            // Resolve the enum class from the type descriptor and create the constant.
            // type_desc is like "Ljava/lang/annotation/RetentionPolicy;" — strip L and ;
            let class_name = type_desc
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(type_desc);
            // S111r19 — load the enum class on demand if not yet loaded.
            // Annotation proxies are materialised eagerly during the
            // declaring class's load, but the enum class referenced by the
            // annotation's element values (e.g. `FilterType` in
            // `@ComponentScan.Filter.type`) often is **not** yet loaded.
            // Previously we fell straight through to the synthetic fallback
            // which writes ordinal=0 — collapsing `FilterType.CUSTOM`
            // (real ordinal 4) onto `ANNOTATION` (ordinal 0) and sending
            // Spring's `ComponentScanAnnotationParser.typeFiltersFor` into
            // the wrong switch case, surfacing as `IllegalArgumentException`
            // wrapped at `ConfigurationClassParser.parse:181`.  Load the
            // class on demand, mirroring the sibling `Class` arm (C29).
            let iae_trace = std::env::var("RUSTJVM_IAE_TRACE").is_ok();
            let enum_cid_opt = ctx.class_id_by_name(class_name).or_else(|| {
                let _ = ctx.load_class(class_name);
                ctx.class_id_by_name(class_name)
            });
            if let Some(enum_cid) = enum_cid_opt {
                let class_mirror = ctx.get_class_mirror(enum_cid);
                let name_str = ctx.create_string(const_name);
                let invoke_res = ctx.invoke(
                    "java/lang/Enum",
                    "valueOf",
                    "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;",
                    &[Value::Object(Some(class_mirror)), Value::Object(Some(name_str))],
                );
                if iae_trace {
                    eprintln!("ANN-ENUM class={class_name} const={const_name} ok={}", invoke_res.as_ref().map(|v| v.is_some()).unwrap_or(false));
                }
                if let Ok(Some(val)) = invoke_res {
                    return val;
                }
            } else if iae_trace {
                eprintln!("ANN-ENUM class={class_name} const={const_name} CLASS-NOT-FOUND");
            }
            // Fallback: allocate a synthetic enum instance with the name and ordinal
            if iae_trace {
                eprintln!("ANN-ENUM FALLBACK class={class_name} const={const_name} ordinal=0");
            }
            let obj = alloc_concurrent_synthetic(ctx, class_name, 2);
            let name_str = ctx.create_string(const_name);
            ctx.set_field(obj, 0, Value::Object(Some(name_str)));
            ctx.set_field(obj, 1, Value::Int(0)); // ordinal
            Value::Object(Some(obj))
        }
        AnnotationElementValue::Class(desc) => {
            // Return Class mirror. If the target class is not yet loaded
            // (common for annotation defaults that reference sibling classes
            // like picocli's NoOpModelTransformer), load it on demand. A null
            // return here causes downstream NullPointerExceptions (C29).
            let iae_trace_cls = std::env::var("RUSTJVM_IAE_TRACE").is_ok();
            if let Some(class_name) = annotation_desc_to_class_name(desc) {
                if let Some(cid) = ctx.class_id_by_name(class_name) {
                    let mirror = ctx.get_class_mirror(cid);
                    if iae_trace_cls { eprintln!("ANN-CLASS desc={desc} class={class_name} already-loaded ok"); }
                    return Value::Object(Some(mirror));
                }
                let load_res = ctx.load_class(class_name);
                if iae_trace_cls { eprintln!("ANN-CLASS desc={desc} class={class_name} load-ok={}", load_res.as_ref().map(|v| v.is_some()).unwrap_or(false)); }
                if let Ok(Some(val)) = load_res {
                    return val;
                }
                if iae_trace_cls { eprintln!("ANN-CLASS desc={desc} class={class_name} RETURNING-NULL"); }
            } else if iae_trace_cls {
                eprintln!("ANN-CLASS desc={desc} NO-CLASS-NAME");
            }
            Value::Object(None)
        }
        AnnotationElementValue::Annotation(nested) => {
            let proxy = create_annotation_proxy(ctx, nested);
            Value::Object(Some(proxy))
        }
        AnnotationElementValue::Array(elems) => {
            // Pick a component class for the array based on the element kind so
            // downstream `instanceof "[Lfoo;"` checks correctly distinguish
            // between e.g. `String[]` and `Annotation[]`. Spring's
            // `AnnotationUtils.adaptValue` runs an `instanceof
            // "[Ljava/lang/annotation/Annotation;"` chain — if the array's
            // component class is bare `Object` (cid=0), the lenient
            // assignability fallback in `array_is_assignable_to`
            // (interpreter.rs `if src_comp == "java/lang/Object" { return
            // true; }`) green-lights the cast and a `String[]` flows into the
            // `Annotation[]` branch, eventually surfacing as
            // `String.annotationType()` NSME inside
            // `retrieveAnnotationAttributes`.
            //
            // Pick by inspecting the first element variant — annotation
            // attribute arrays are homogeneous per JLS §9.6.1.
            //
            // S111r18 — for nested-annotation arrays, use the annotation
            // interface type (e.g. `F4` for `@CScan(excludeFilters=@F4...)`)
            // as the component class, NOT the bare `AnnotationProxy` synthetic.
            // Spring's `MergedAnnotation.adaptForAttribute` walks
            // `returnType.componentType().isAnnotation()` — when our array
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
            use rustjvm_native_api::AnnotationElementValue as AEV;
            // S111r19 — when the array is **empty** (no first element to
            // probe), fall back to the caller-provided method return-type
            // descriptor.  This recovers the right component class for
            // empty `String[]` / `Class[]` defaults like `@Filter.pattern()`
            // = `{}`, which would otherwise become an `Object[]` and trip
            // Spring's `AnnotationUtils.adaptValue` Annotation[]-detection
            // (the lenient `Object` fallback in `array_is_assignable`).
            let comp_name_owned: String = match elems.first() {
                Some(AEV::StringVal(_)) => "java/lang/String".to_string(),
                Some(AEV::Class(_)) => "java/lang/Class".to_string(),
                Some(AEV::Annotation(nested)) => annotation_desc_to_class_name(
                    &nested.type_descriptor,
                )
                .map(|s| s.to_string())
                .unwrap_or_else(|| "java/lang/annotation/AnnotationProxy".to_string()),
                Some(AEV::Enum(type_desc, _)) => type_desc
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .unwrap_or("java/lang/Enum")
                    .to_string(),
                // Primitive arrays in annotations (`int[]`, `boolean[]`, etc.)
                // are still allocated as boxed wrapper arrays here per
                // pre-existing behaviour — pick the wrapper class.
                Some(AEV::Int(_)) => "java/lang/Integer".to_string(),
                Some(AEV::Long(_)) => "java/lang/Long".to_string(),
                Some(AEV::Float(_)) => "java/lang/Float".to_string(),
                Some(AEV::Double(_)) => "java/lang/Double".to_string(),
                None => {
                    // Empty array — derive component from method return type.
                    if let Some(rd) = return_type_desc {
                        if let Some(comp) = rd.strip_prefix('[') {
                            if let Some(stripped) =
                                comp.strip_prefix('L').and_then(|s| s.strip_suffix(';'))
                            {
                                stripped.to_string()
                            } else if comp.len() == 1 && "ZBCSIJFD".contains(&comp[..1]) {
                                // Empty primitive array — boxed wrapper
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
            let comp_cid = ctx
                .class_id_by_name(&comp_name_owned)
                .or_else(|| {
                    let _ = ctx.load_class(&comp_name_owned);
                    ctx.class_id_by_name(&comp_name_owned)
                })
                .unwrap_or(rustjvm_types::ClassId::new(0));
            let arr = ctx.new_ref_array(comp_cid, elems.len());
            // Round 18: derive the per-element return-type descriptor from
            // the array descriptor (strip leading `[`) so primitive elements
            // box into the correct wrapper (Z/B/C/S → Boolean/Byte/Char/Short
            // instead of always Integer).
            let elem_desc: Option<String> = return_type_desc
                .and_then(|rd| rd.strip_prefix('['))
                .map(|s| s.to_string());
            for (i, elem) in elems.iter().enumerate() {
                let v = annotation_element_to_java_typed(ctx, elem, elem_desc.as_deref());
                ctx.set_array_element(arr, i, v);
            }
            Value::Object(Some(arr))
        }
    }
}

/// Build an Annotation[] array from annotation data.
fn build_annotation_array(
    ctx: &mut dyn NativeContext,
    annotations: &[rustjvm_native_api::AnnotationData],
) -> ObjectRef {
    use rustjvm_types::ClassId;
    let arr = ctx.new_ref_array(ClassId::new(0), annotations.len());
    for (i, ann) in annotations.iter().enumerate() {
        let proxy = create_annotation_proxy(ctx, ann);
        ctx.set_array_element(arr, i, Value::Object(Some(proxy)));
    }
    arr
}

/// Class.getDeclaredAnnotations() — only this class's own annotations.
pub(crate) fn native_class_get_declared_annotations(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let annotations = ctx.class_annotations(class_id);
    if std::env::var("RUSTJVM_ANN_TRACE").is_ok() {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        if cn.contains("SpringBootApplication") || cn.contains("EnableAutoConfiguration") || cn.contains("SpringBootConfiguration") {
            eprintln!("[GDA] {} -> {} annotations", cn, annotations.len());
            for a in &annotations { eprintln!("    {}", a.type_descriptor); }
        }
    }
    let arr = build_annotation_array(ctx, &annotations);
    Ok(Some(Value::Object(Some(arr))))
}

/// Class.getAnnotations() — includes @Inherited annotations from superclasses.
pub(crate) fn native_class_get_annotations(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };

    let mut annotations = ctx.class_annotations(class_id);

    // Collect type descriptors already present on this class
    let mut seen: std::collections::HashSet<String> = annotations.iter().map(|a| a.type_descriptor.clone()).collect();

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

    let arr = build_annotation_array(ctx, &annotations);
    Ok(Some(Value::Object(Some(arr))))
}

/// Check if an annotation type is marked with @Inherited.
fn is_inherited_annotation(ctx: &mut dyn NativeContext, ann_type_desc: &str) -> bool {
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
    meta_annotations.iter().any(|a| a.type_descriptor == "Ljava/lang/annotation/Inherited;")
}

/// Class.getDeclaredAnnotation(Class) — only this class's own annotations.
pub(crate) fn native_class_get_declared_annotation(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
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
            let proxy = create_annotation_proxy(ctx, ann);
            return Ok(Some(Value::Object(Some(proxy))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Class.getAnnotation(Class) — searches superclass chain for @Inherited.
pub(crate) fn native_class_get_annotation(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
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
            let proxy = create_annotation_proxy(ctx, ann);
            return Ok(Some(Value::Object(Some(proxy))));
        }
    }

    // Walk superclass chain for @Inherited annotations
    if is_inherited_annotation(ctx, &target_desc) {
        let mut current = ctx.superclass_of(class_id);
        while let Some(super_id) = current {
            let super_anns = ctx.class_annotations(super_id);
            for ann in &super_anns {
                if ann.type_descriptor == target_desc {
                    let proxy = create_annotation_proxy(ctx, ann);
                    return Ok(Some(Value::Object(Some(proxy))));
                }
            }
            current = ctx.superclass_of(super_id);
        }
    }

    Ok(Some(Value::Object(None)))
}

/// Class.isAnnotationPresent(Class) — searches superclass chain for @Inherited.
pub(crate) fn native_class_is_annotation_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
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

/// Class.isAnnotation() — checks if the class itself is an annotation type
pub(crate) fn native_class_is_annotation(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

/// Class.getAnnotationsByType(Class) / getDeclaredAnnotationsByType(Class)
pub(crate) fn native_class_get_annotations_by_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let ann_class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let ann_class_id = match mirror_class_id(ctx, ann_class_mirror) {
        Some(id) => id,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let ann_class_name = match ctx.class_name_of_id(ann_class_id) {
        Some(n) => n,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let target_desc = format!("L{};", ann_class_name);
    let annotations = ctx.class_annotations(class_id);

    // Collect directly-matching annotations
    let mut matching: Vec<_> = annotations
        .iter()
        .filter(|a| a.type_descriptor == target_desc)
        .cloned()
        .collect();

    // @Repeatable container unwrapping: if no direct matches, look for the
    // container annotation. The annotation type (ann_class_id) should itself
    // have a @Repeatable annotation whose value() is the container class.
    // We check the annotation type's own annotations for @Repeatable.
    if matching.is_empty() {
        let ann_type_annotations = ctx.class_annotations(ann_class_id);
        let repeatable_desc = "Ljava/lang/annotation/Repeatable;";
        if let Some(repeatable_ann) = ann_type_annotations
            .iter()
            .find(|a| a.type_descriptor == repeatable_desc)
        {
            // The @Repeatable annotation has a single element "value" which is a Class
            // descriptor for the container annotation type.
            if let Some((_, rustjvm_native_api::AnnotationElementValue::Class(container_desc))) =
                repeatable_ann.elements.iter().find(|(name, _)| name == "value")
            {
                // Find the container annotation on the target class
                for ann in &annotations {
                    if ann.type_descriptor == *container_desc {
                        // The container's value() element is an Array of nested annotations
                        if let Some((_, rustjvm_native_api::AnnotationElementValue::Array(elems))) =
                            ann.elements.iter().find(|(name, _)| name == "value")
                        {
                            for elem in elems {
                                if let rustjvm_native_api::AnnotationElementValue::Annotation(nested) = elem {
                                    if nested.type_descriptor == target_desc {
                                        matching.push(nested.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let arr = ctx.new_ref_array(ClassId::new(0), matching.len());
    for (i, ann) in matching.iter().enumerate() {
        let proxy = create_annotation_proxy(ctx, ann);
        ctx.set_array_element(arr, i, Value::Object(Some(proxy)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Helper: extract declaring class ID and field name from a Field reflection object.
pub(crate) fn field_class_and_name(
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
    Some((class_id, name))
}

/// Helper: extract declaring class ID, method name, and descriptor from a Method reflection object.
pub(crate) fn method_class_name_desc(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
) -> Option<(ClassId, String, String)> {
    let mirror = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let class_id = mirror_class_id(ctx, mirror)?;
    let name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    // Descriptor is stashed in our RustJVM extra slot (C6: not a real JDK field).
    let desc = read_method_descriptor(ctx, method_obj)?;
    Some((class_id, name, desc))
}

/// Field.getAnnotations() / Field.getDeclaredAnnotations()
pub(crate) fn native_field_get_annotations(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let (class_id, field_name) = match field_class_and_name(ctx, this) {
        Some(v) => v,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let annotations = ctx.field_annotations(class_id, &field_name);
    let arr = build_annotation_array(ctx, &annotations);
    Ok(Some(Value::Object(Some(arr))))
}

/// Field.isAnnotationPresent(Class)
pub(crate) fn native_field_is_annotation_present(
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

/// Field.getAnnotation(Class) — returns a single annotation proxy or null.
pub(crate) fn native_field_get_annotation(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
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
    for ann in &annotations {
        if ann.type_descriptor == target_desc {
            let proxy = create_annotation_proxy(ctx, ann);
            return Ok(Some(Value::Object(Some(proxy))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Method.getAnnotations() / Method.getDeclaredAnnotations()
pub(crate) fn native_method_get_annotations(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let annotations = ctx.method_annotations(class_id, &method_name, &method_desc);
    if std::env::var("RUSTJVM_ANN_TRACE").is_ok() {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        if cn.contains("SpringBootApplication") || cn.contains("EnableAutoConfiguration") {
            eprintln!("[MGA] {}.{}{} -> {} method-anns", cn, method_name, method_desc, annotations.len());
            for a in &annotations {
                eprintln!("    {} elements={}", a.type_descriptor, a.elements.len());
                for (en, ev) in &a.elements {
                    eprintln!("      {} -> {:?}", en, ev);
                }
            }
        }
    }
    let arr = build_annotation_array(ctx, &annotations);
    Ok(Some(Value::Object(Some(arr))))
}

/// Method.isAnnotationPresent(Class)
pub(crate) fn native_method_is_annotation_present(
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
pub(crate) fn native_method_get_annotation(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ann_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
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
    if std::env::var("RUSTJVM_ANN_TRACE").is_ok() {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        if cn.contains("SpringBootApplication") {
            eprintln!("[GMA] {}.{}{} target={} -> {} method-anns", cn, method_name, method_desc, target_desc, annotations.len());
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
            let proxy = create_annotation_proxy(ctx, ann);
            return Ok(Some(Value::Object(Some(proxy))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Annotation.annotationType() — returns the Class mirror of the annotation type
pub(crate) fn native_annotation_annotation_type(
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

/// Method.getParameterAnnotations() — returns Annotation[][] (one row per parameter).
pub(crate) fn native_method_get_parameter_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let empty = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
    };
    let param_annotations = ctx.method_parameter_annotations(class_id, &method_name, &method_desc);
    if param_annotations.is_empty() {
        // Return an Annotation[param_count][0] — count params from descriptor
        let param_count = count_method_params(&method_desc);
        let outer = ctx.new_ref_array(ClassId::new(0), param_count);
        for i in 0..param_count {
            let inner = ctx.new_ref_array(ClassId::new(0), 0);
            ctx.set_array_element(outer, i, Value::Object(Some(inner)));
        }
        return Ok(Some(Value::Object(Some(outer))));
    }
    let outer = ctx.new_ref_array(ClassId::new(0), param_annotations.len());
    for (i, anns) in param_annotations.iter().enumerate() {
        let inner = build_annotation_array(ctx, anns);
        ctx.set_array_element(outer, i, Value::Object(Some(inner)));
    }
    Ok(Some(Value::Object(Some(outer))))
}

/// Count the number of parameters in a method descriptor.
fn count_method_params(desc: &str) -> usize {
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
                // array dimension — don't count, advance to element type
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

/// Class.getTypeParameters() — returns TypeVariable[] from class signature.
pub(crate) fn native_class_get_type_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let sig_str = match ctx.class_signature(class_id) {
        Some(s) => s,
        None => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let class_sig = match crate::generics::parse_class_signature(&sig_str) {
        Some(s) => s,
        None => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let arr = ctx.new_ref_array(ClassId::new(0), class_sig.type_params.len());
    for (i, tp) in class_sig.type_params.iter().enumerate() {
        let tv = crate::generics::type_param_to_java(ctx, tp);
        ctx.set_array_element(arr, i, tv);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Class.getGenericSuperclass() — returns Type for the generic superclass.
pub(crate) fn native_class_get_generic_superclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    // If class has a Signature attribute, parse it for the generic superclass
    if let Some(sig_str) = ctx.class_signature(class_id) {
        if let Some(class_sig) = crate::generics::parse_class_signature(&sig_str) {
            let val = crate::generics::type_sig_to_java(ctx, &class_sig.super_class);
            // If signature resolution succeeded, return it
            if !matches!(val, Value::Object(None)) {
                return Ok(Some(val));
            }
        }
    }
    // Fallback: return the raw superclass as a Class mirror
    if let Some(super_id) = ctx.superclass_of(class_id) {
        let mirror = ctx.get_class_mirror(super_id);
        Ok(Some(Value::Object(Some(mirror))))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

/// Class.getGenericInterfaces() — returns Type[] for generic interfaces.
pub(crate) fn native_class_get_generic_interfaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // If class has a Signature attribute, parse it for generic interfaces
    if let Some(sig_str) = ctx.class_signature(class_id) {
        if let Some(class_sig) = crate::generics::parse_class_signature(&sig_str) {
            if !class_sig.interfaces.is_empty() {
                let arr = ctx.new_ref_array(ClassId::new(0), class_sig.interfaces.len());
                for (i, iface) in class_sig.interfaces.iter().enumerate() {
                    let val = crate::generics::type_sig_to_java(ctx, iface);
                    ctx.set_array_element(arr, i, val);
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
    }
    // Fallback: return empty array
    let arr = ctx.new_ref_array(ClassId::new(0), 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// Method.getGenericParameterTypes() — returns Type[] from method signature.
pub(crate) fn native_method_get_generic_param_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &method_desc) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            let arr = ctx.new_ref_array(ClassId::new(0), method_sig.param_types.len());
            for (i, pt) in method_sig.param_types.iter().enumerate() {
                let val = crate::generics::type_sig_to_java(ctx, pt);
                ctx.set_array_element(arr, i, val);
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // Fallback: return raw parameter types from the JDK `parameterTypes` field.
    let param_types = ctx.get_field_by_name(this, "parameterTypes");
    Ok(Some(param_types))
}

/// Method.getGenericReturnType() — returns Type from method signature.
pub(crate) fn native_method_get_generic_return_type(
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
    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &method_desc) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            let val = crate::generics::type_sig_to_java(ctx, &method_sig.return_type);
            return Ok(Some(val));
        }
    }
    // Fallback: return raw return type from the JDK `returnType` field.
    Ok(Some(ctx.get_field_by_name(this, "returnType")))
}

/// Method.getTypeParameters() — returns TypeVariable[] from method signature.
pub(crate) fn native_method_get_type_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let (class_id, method_name, method_desc) = match method_class_name_desc(ctx, this) {
        Some(v) => v,
        None => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &method_desc) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            if !method_sig.type_params.is_empty() {
                let arr = ctx.new_ref_array(ClassId::new(0), method_sig.type_params.len());
                for (i, tp) in method_sig.type_params.iter().enumerate() {
                    let tv = crate::generics::type_param_to_java(ctx, tp);
                    ctx.set_array_element(arr, i, tv);
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
    }
    let arr = ctx.new_ref_array(ClassId::new(0), 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// Field.getGenericType() — returns Type from field signature.
///
/// WP2.1 (FAIL-10): Parses the JVMS §4.7.9 Signature attribute on the field
/// (when present, e.g. `Ljava/util/List<Ljava/lang/String;>;` for
/// `List<String> list`) into a `ParameterizedType` runtime object. When no
/// Signature attribute is present (non-generic field), falls back to the
/// raw `Class<?>` returned by `Field.getType()`.
pub(crate) fn native_field_get_generic_type(
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
    if let Some(sig_str) = ctx.field_signature(class_id, &field_name) {
        if let Some(field_sig) = crate::generics::parse_field_signature(&sig_str) {
            let val = crate::generics::type_sig_to_java(ctx, &field_sig);
            return Ok(Some(val));
        }
    }
    // Fallback: when no Signature attribute, getGenericType() ≡ getType().
    // The `type` field is the Class<?> mirror at the JDK-native layout slot.
    Ok(Some(ctx.get_field_by_name(this, "type")))
}

// --- java.lang.reflect.Modifier ---

pub(crate) fn modifier_check(_ctx: &mut dyn NativeContext, args: &[Value], mask: i32) -> MethodCallResult {
    let flags = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(if (flags & mask) != 0 { 1 } else { 0 })))
}

pub(crate) fn native_modifier_is_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0001)
}

pub(crate) fn native_modifier_is_private(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0002)
}

pub(crate) fn native_modifier_is_protected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0004)
}

pub(crate) fn native_modifier_is_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0008)
}

pub(crate) fn native_modifier_is_final(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0010)
}

pub(crate) fn native_modifier_is_abstract(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0400)
}

// ---------------------------------------------------------------------------

pub(crate) fn native_temp_print_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(&value) = args.first() {
        ctx.record_printed_value(value);
    }
    Ok(None)
}

pub(crate) fn native_temp_print_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(&value) = args.first() {
        ctx.record_printed_value(value);
    }
    Ok(None)
}

// ===========================================================================
// Modifier extras
// ===========================================================================

pub(crate) fn native_modifier_is_synchronized(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    modifier_check(ctx, args, 0x0020)
}

pub(crate) fn native_modifier_is_volatile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0040)
}

pub(crate) fn native_modifier_is_transient(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0080)
}

pub(crate) fn native_modifier_is_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0100)
}

pub(crate) fn native_modifier_is_interface(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0200)
}

pub(crate) fn native_modifier_is_strict(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    modifier_check(ctx, args, 0x0800)
}

pub(crate) fn native_modifier_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let flags = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let mut parts = Vec::new();
    if (flags & 0x0001) != 0 {
        parts.push("public");
    }
    if (flags & 0x0002) != 0 {
        parts.push("private");
    }
    if (flags & 0x0004) != 0 {
        parts.push("protected");
    }
    if (flags & 0x0008) != 0 {
        parts.push("static");
    }
    if (flags & 0x0010) != 0 {
        parts.push("final");
    }
    if (flags & 0x0020) != 0 {
        parts.push("synchronized");
    }
    if (flags & 0x0040) != 0 {
        parts.push("volatile");
    }
    if (flags & 0x0080) != 0 {
        parts.push("transient");
    }
    if (flags & 0x0100) != 0 {
        parts.push("native");
    }
    if (flags & 0x0200) != 0 {
        parts.push("interface");
    }
    if (flags & 0x0400) != 0 {
        parts.push("abstract");
    }
    if (flags & 0x0800) != 0 {
        parts.push("strictfp");
    }
    let s = parts.join(" ");
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_modifier_class_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // PUBLIC | PROTECTED | PRIVATE | ABSTRACT | STATIC | FINAL | STRICT
    Ok(Some(Value::Int(
        0x0001 | 0x0004 | 0x0002 | 0x0400 | 0x0008 | 0x0010 | 0x0800,
    )))
}

pub(crate) fn native_modifier_interface_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(
        0x0001 | 0x0004 | 0x0002 | 0x0400 | 0x0008 | 0x0800,
    )))
}

pub(crate) fn native_modifier_field_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(
        0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0010 | 0x0040 | 0x0080,
    )))
}

pub(crate) fn native_modifier_method_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(
        0x0001 | 0x0002 | 0x0004 | 0x0400 | 0x0008 | 0x0010 | 0x0020 | 0x0100 | 0x0800,
    )))
}

pub(crate) fn native_modifier_constructor_modifiers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0x0001 | 0x0002 | 0x0004)))
}

// ===========================================================================
// Class extras
// ===========================================================================

pub(crate) fn native_class_get_component_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            if std::env::var("RUSTJVM_DBG_COMPONENT_TYPE").is_ok() {
                eprintln!("[CT-DBG] getComponentType receiver=null");
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
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
        // Primitive types don't have loadable classes — use primitive_class_mirror
        match comp_name {
            "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short" | "void" => {
                let mirror = ctx.primitive_class_mirror(comp_name);
                return Ok(Some(Value::Object(Some(mirror))));
            }
            _ => {
                if let Ok(cid) = ctx.ensure_class_initialized(comp_name) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
                // SB3.2 — when the component class can't be loaded
                // (e.g. nested array `[[L...;`, or absent class), still
                // return a non-null Class mirror so callers that assume
                // `array.componentType() != null` (Spring's
                // `ConstructorResolver.resolveAutowiredArgument` does
                // `Array.newInstance(type.componentType(), 0)`) do not NPE.
                return Ok(Some(Value::Object(Some(synthetic_class_mirror(ctx, comp_name)))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_class_get_package_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let pkg = if let Some(pos) = name.rfind('/') {
        name[..pos].replace('/', ".")
    } else {
        String::new()
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&pkg)))))
}

// ---------------------------------------------------------------------------
// T19_H10_GET_PACKAGE: real `Class.getPackage()` — was `native_return_null`,
// which broke any code path that did
// `Foo.class.getPackage().getImplementationVersion()` (a popular way to
// discover the version of a library at runtime — Hibernate, Logback, JBoss
// LogManager all do this in `<clinit>` and would previously NPE on the
// `.getImplementationVersion()` deref).
//
// We synthesise a `java.lang.Package` object with:
//   * `name` — the dotted package name (e.g. `org.keycloak.common`).
//   * `implementationTitle` — the manifest's `Implementation-Title`, or null.
//   * `implementationVersion` — the manifest's `Implementation-Version`,
//     or null when the class is not jar-loaded.
//   * `specificationTitle/Version/Vendor` — the matching `Specification-*`
//     attributes (often unset on modern jars; we surface null in that case).
//
// The lookup is best-effort: if the class is loaded from the boot classpath
// (no manifest) or from a directory, we return a Package with name set and
// every other field null. That matches HotSpot, which always returns a
// non-null Package for a named class even when the manifest is missing.
//
// Layout: real-JDK `java.lang.Package` extends `java.lang.NamedPackage`;
// we write fields by name to insulate from layout drift, and additionally
// keep slot 0 (the synthetic-mode "name" slot) populated for
// pre-class-loaded paths.
// ---------------------------------------------------------------------------

/// Read a manifest attribute by name from the class's source jar, if any.
/// Returns `None` for classes loaded from a directory or the boot path.
///
/// Supports three CodeSource URL forms:
///   * `file:/C:/.../foo.jar`                              — plain jar
///   * `jar:file:/C:/.../outer.jar!/BOOT-INF/lib/inner.jar!/` — Spring Boot 2.x
///   * `jar:nested:/C:/.../outer.jar/!BOOT-INF/lib/inner.jar!/` — Spring Boot 3.x
///
/// For the nested forms, opens the outer jar, extracts the inner jar entry
/// to a byte buffer, reads `META-INF/MANIFEST.MF` from the inner zip, and
/// parses out the requested attribute. This is what unblocks
/// `SpringBootVersion.class.getPackage().getImplementationVersion()` for
/// Spring Boot fat-jar deployments where the class lives in a nested jar.
fn t19_h10_class_manifest_attr(
    ctx: &mut dyn NativeContext,
    class_id: rustjvm_types::ClassId,
    attr: &str,
) -> Option<String> {
    let url = ctx.class_code_base(class_id)?;

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
                if let Some(val) = nested_jar_manifest_attr(&outer_pb, inner_entry, attr) {
                    return Some(val);
                }
            }
        }
        // Fall through to None if the nested form failed to resolve.
        return None;
    }

    // Plain `file:` URL (single jar).
    let path = url.strip_prefix("file:").unwrap_or(&url);
    let path = path.trim_start_matches('/');
    let path = if cfg!(windows) {
        std::path::PathBuf::from(path)
    } else {
        std::path::PathBuf::from(format!("/{}", path))
    };
    if !path.is_file() {
        return None;
    }
    let manifest = rustjvm_classloading::ClassPath::read_jar_manifest(&path)?;
    manifest.attributes.get(attr).cloned()
}

/// Open the outer jar, extract the inner-jar entry into memory, and read
/// the requested `META-INF/MANIFEST.MF` attribute from inside the inner jar.
/// Returns `None` on any I/O / format failure (best-effort).
fn nested_jar_manifest_attr(
    outer_jar: &std::path::Path,
    inner_entry: &str,
    attr: &str,
) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(outer_jar).ok()?;
    let mut outer = zip::ZipArchive::new(file).ok()?;
    // Inner entry path inside the outer zip — strip any leading `/`.
    let entry_name = inner_entry.trim_start_matches('/');
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
    // Parse MANIFEST.MF main attributes (no continuation-line handling for
    // the simple `Implementation-Version: X.Y.Z` cases we care about).
    for line in mf_str.lines() {
        if let Some((k, v)) = line.split_once(": ") {
            if k.trim().eq_ignore_ascii_case(attr) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Real `Class.getPackage()` native — returns a `java.lang.Package` mirror
/// or null when the class has no resolvable package (primitive / array of
/// primitive). Always non-null for a real reference type.
pub(crate) fn native_class_get_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    // Array of primitive (e.g. `[I`) and bare primitives have no package.
    if name.is_empty() || (name.starts_with('[') && !name.contains('/')) {
        return Ok(Some(Value::Object(None)));
    }
    let pkg_name = if let Some(pos) = name.rfind('/') {
        name[..pos].replace('/', ".")
    } else {
        // Default package — return a Package object whose name is the empty
        // string, matching JDK 25 (`Class.forName("Foo").getPackage()`
        // yields a Package with `getName().equals("")`).
        String::new()
    };
    // Read manifest attributes (best-effort).
    let (impl_title, impl_version, spec_title, spec_version, spec_vendor, impl_vendor) =
        if let Some(class_id) = mirror_class_id(ctx, this) {
            (
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Vendor"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Vendor"),
            )
        } else {
            (None, None, None, None, None, None)
        };
    let pkg = alloc_concurrent_synthetic(ctx, "java/lang/Package", 12);
    // Slot 0: name (synthetic-mode layout used by `getPackageName`/`getName`
    // shims pre-real-class-load). Same write goes by-name for real JDK.
    let name_str = ctx.create_string(&pkg_name);
    ctx.set_field(pkg, 0, Value::Object(Some(name_str)));
    ctx.set_field_by_name(pkg, "name", Value::Object(Some(name_str)));
    // Manifest-derived attributes.
    let write_optional = |ctx: &mut dyn NativeContext, slot: usize, field: &str, val: Option<String>| {
        let obj = match val {
            Some(s) => Value::Object(Some(ctx.create_string(&s))),
            None => Value::Object(None),
        };
        ctx.set_field(pkg, slot, obj);
        ctx.set_field_by_name(pkg, field, obj);
    };
    write_optional(ctx, 1, "specTitle", spec_title);
    write_optional(ctx, 2, "specVersion", spec_version);
    write_optional(ctx, 3, "specVendor", spec_vendor);
    write_optional(ctx, 4, "implTitle", impl_title);
    write_optional(ctx, 5, "implVersion", impl_version);
    write_optional(ctx, 6, "implVendor", impl_vendor);
    Ok(Some(Value::Object(Some(pkg))))
}

// ---------------------------------------------------------------------------
// I2 — ClassLoader package-management overrides
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
// Fix: register our own native bodies for the three methods below so they
// short-circuit before touching the null `packages` field. They mirror the
// `cl_get_defined_package` synthetic-mode native (returns null) and the
// `native_class_get_package` synthetic Package builder so that
// `postDefineClass` and `Class.getPackage()` keep working without depending
// on `packages` being non-null.

/// `ClassLoader.getDefinedPackage(String name) -> Package` — returns null
/// (no package is defined on this classloader). JDK semantics: returning
/// null is correct when the classloader has not previously defined a
/// package by that name. ByteBuddy's `Resolver$ForModuleSystem.accept`
/// already null-checks the result, so returning null is a clean exit.
pub(crate) fn i2_classloader_get_defined_package(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `ClassLoader.getDefinedPackages() -> Package[]` — returns an empty array.
pub(crate) fn i2_classloader_get_defined_packages(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let empty = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(empty))))
}

/// Internal helper: synthesise a `java/lang/Package` whose `name` slot is
/// set to the requested package name (dotted). Mirrors the layout used by
/// `native_class_get_package` so callers that subsequently invoke
/// `Package.getName()` see the right value.
fn i2_alloc_synthetic_package(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    let pkg = alloc_concurrent_synthetic(ctx, "java/lang/Package", 12);
    let name_str = ctx.create_string(name);
    ctx.set_field(pkg, 0, Value::Object(Some(name_str)));
    ctx.set_field_by_name(pkg, "name", Value::Object(Some(name_str)));
    pkg
}

/// `ClassLoader.getNamedPackage(String packageName, Module m) -> NamedPackage`
/// (private, called from `postDefineClass`). The JDK implementation reads
/// the `packages` map and inserts a new `NamedPackage` if absent; we
/// synthesise a fresh one without ever touching `packages`. Matches
/// `Package` (which extends `NamedPackage`) so the cast at the call site
/// is benign — callers ignore the result anyway (the value is `pop`ped
/// in `postDefineClass`).
pub(crate) fn i2_classloader_get_named_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let pkg = i2_alloc_synthetic_package(ctx, &pkg_name);
    // Persist the module reference too so `Package.module()` returns the
    // caller-supplied module if it does get queried later.
    if let Some(module_val) = args.get(2).copied() {
        ctx.set_field_by_name(pkg, "module", module_val);
    }
    Ok(Some(Value::Object(Some(pkg))))
}

/// `ClassLoader.definePackage(String name, Module m) -> Package` — same
/// shape as `getNamedPackage` but typed as `Package`. The JDK's bytecode
/// also reads the `packages` map; we bypass it entirely.
pub(crate) fn i2_classloader_define_package_string_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let pkg = i2_alloc_synthetic_package(ctx, &pkg_name);
    if let Some(module_val) = args.get(2).copied() {
        ctx.set_field_by_name(pkg, "module", module_val);
    }
    Ok(Some(Value::Object(Some(pkg))))
}

/// `ClassLoader.definePackage(Class<?>) -> Package` — derives the package
/// name from `c.getPackageName()` and synthesises a Package. This is the
/// single-arg overload called by `Class.getPackage()`'s JDK Java body
/// when the native override (registered separately for `Class.getPackage`)
/// isn't taken.
///
/// Also populates the manifest-derived fields (`implementationTitle`,
/// `implementationVersion`, `specification*`, `implementationVendor`) from
/// the class's source jar — same data path as `native_class_get_package`.
/// Without this, callers like `Foo.class.getPackage().getImplementationVersion()`
/// in real-JDK mode see slot 5 = null (because real `Class.getPackage()`
/// delegates here), even though the synthetic-jdk override correctly
/// populated those fields. This caused Spring Boot 2.x to NPE in
/// `SpringBootVersion.determineSpringBootVersion()` on the JarURLConnection
/// fallback path.
pub(crate) fn i2_classloader_define_package_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (ClassLoader), args[1] = c (Class)
    let class_arg = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = mirror_class_name(ctx, class_arg).unwrap_or_default();
    if class_name.is_empty() || (class_name.starts_with('[') && !class_name.contains('/')) {
        return Ok(Some(Value::Object(None)));
    }
    let pkg_name = if let Some(pos) = class_name.rfind('/') {
        class_name[..pos].replace('/', ".")
    } else {
        String::new()
    };
    // Read manifest attributes (best-effort) from the class's source jar.
    let (impl_title, impl_version, spec_title, spec_version, spec_vendor, impl_vendor) =
        if let Some(class_id) = mirror_class_id(ctx, class_arg) {
            (
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Vendor"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Vendor"),
            )
        } else {
            (None, None, None, None, None, None)
        };
    let pkg = i2_alloc_synthetic_package(ctx, &pkg_name);
    let write_optional = |ctx: &mut dyn NativeContext, slot: usize, field: &str, val: Option<String>| {
        let obj = match val {
            Some(s) => Value::Object(Some(ctx.create_string(&s))),
            None => Value::Object(None),
        };
        ctx.set_field(pkg, slot, obj);
        ctx.set_field_by_name(pkg, field, obj);
    };
    write_optional(ctx, 1, "specTitle", spec_title);
    write_optional(ctx, 2, "specVersion", spec_version);
    write_optional(ctx, 3, "specVendor", spec_vendor);
    write_optional(ctx, 4, "implTitle", impl_title);
    write_optional(ctx, 5, "implVersion", impl_version);
    write_optional(ctx, 6, "implVendor", impl_vendor);
    Ok(Some(Value::Object(Some(pkg))))
}

/// Wire up I2 ClassLoader package overrides. Invoked from
/// `lang_invoke::register_t28_method_handle_completeness` so they land in
/// both real-JDK and synthetic-jdk registration paths without any
/// edits to `lib.rs` / `vm_init.rs` (per I2 surface rules).
pub fn i2_register_classloader_package_natives(
    r: &mut rustjvm_native_api::NativeMethodRegistry,
) {
    let cl = "java/lang/ClassLoader";
    r.register(
        cl,
        "getDefinedPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
        i2_classloader_get_defined_package,
    );
    r.register(
        cl,
        "getDefinedPackages",
        "()[Ljava/lang/Package;",
        i2_classloader_get_defined_packages,
    );
    // `ClassLoader.getPackages()` — real JDK bytecode is
    // `return packages().toArray(Package[]::new)`. In our boot the stream
    // pipeline leaks a `ReferencePipeline$Head` into the caller's local
    // typed as `Package[]`, NPE-ing on arraylength inside
    // `org/jboss/modules/ConcurrentClassLoader.<clinit>` (WildFly 39 boot).
    // Override with empty array (same shape as `getDefinedPackages`).
    r.register(
        cl,
        "getPackages",
        "()[Ljava/lang/Package;",
        i2_classloader_get_defined_packages,
    );
    // `Package.getPackages()` is static and delegates to
    // `ClassLoader.getClassLoader(Reflection.getCallerClass()).getPackages()`.
    // Override here as well so direct callers (the WildFly boot path) get an
    // empty array even if the static delegation pulls a different ClassLoader
    // mirror.
    r.register(
        "java/lang/Package",
        "getPackages",
        "()[Ljava/lang/Package;",
        i2_classloader_get_defined_packages,
    );
    r.register(
        cl,
        "getNamedPackage",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/NamedPackage;",
        i2_classloader_get_named_package,
    );
    r.register(
        cl,
        "definePackage",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Package;",
        i2_classloader_define_package_string_module,
    );
    r.register(
        cl,
        "definePackage",
        "(Ljava/lang/Class;)Ljava/lang/Package;",
        i2_classloader_define_package_class,
    );
    // I2: see `i2_classloader_check_certs` doc-comment.
    r.register(
        cl,
        "checkCerts",
        "(Ljava/lang/String;Ljava/security/CodeSource;)V",
        i2_classloader_check_certs,
    );
}

/// `ClassLoader.checkCerts(String, CodeSource)` (private) — no-op. The JDK's
/// implementation reads the `package2certs: ConcurrentHashMap` field which
/// also stays null on user-instantiated subclasses, NPE-ing inside
/// `preDefineClass`. We skip the check entirely (we don't enforce
/// signer-coherency anyway).
pub(crate) fn i2_classloader_check_certs(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

pub(crate) fn native_class_is_enum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let flags = ctx.class_access_flags(class_id);
    // ACC_ENUM = 0x4000
    Ok(Some(Value::Int(if (flags & 0x4000) != 0 { 1 } else { 0 })))
}

pub(crate) fn native_class_get_canonical_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let canonical = name.replace(['/', '$'], ".");
    Ok(Some(Value::Object(Some(ctx.create_string(&canonical)))))
}

pub(crate) fn native_class_get_type_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let type_name = name.replace('/', ".");
    Ok(Some(Value::Object(Some(ctx.create_string(&type_name)))))
}

pub(crate) fn native_class_get_enum_constants(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Real implementation: read the synthetic `$VALUES` static field
    // populated by the enum's `<clinit>`, then return a fresh Object[]
    // clone so callers (e.g. EnumMap.getKeyUniverse → arraylength) see a
    // non-null, fully-populated array.
    //
    // This is the target of both `Class.getEnumConstants()` and the
    // package-private `Class.getEnumConstantsShared()` — both bytecode
    // paths in the real JDK ultimately need the enum's `$VALUES` array,
    // and our earlier stub that returned an empty array broke every
    // `EnumMap.<init>(Class)` call (e.g. StreamOpFlag.<clinit>).
    use rustjvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    // Non-enum classes must return null (per JDK spec).
    let flags = ctx.class_access_flags(class_id);
    if (flags & 0x4000) == 0 {
        return Ok(Some(Value::Object(None)));
    }
    // Ensure the enum's <clinit> has run so `$VALUES` is populated.
    let class_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    if let Err(_e) = ctx.ensure_class_initialized(&class_name) {
        // Fall through — we can still try to read $VALUES if it was
        // populated before the failure (common after silent-swallow).
    }
    // Read $VALUES static field.
    let idx = match ctx.static_field_index_by_name(class_id, "$VALUES") {
        Some(i) => i,
        None => return Ok(Some(Value::Object(None))),
    };
    let values_val = ctx.get_static_field(class_id, idx);
    let src_arr = match values_val {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Clone into a new Object[] so callers can mutate without affecting
    // the enum's backing array (matches `getEnumConstantsShared().clone()`
    // semantics used by `getEnumConstants`).
    let len = ctx.array_length(src_arr);
    let out = ctx.new_array(ArrayElementType::Reference, len);
    for i in 0..len {
        let v = ctx.get_array_element(src_arr, i);
        ctx.set_array_element(out, i, v);
    }
    Ok(Some(Value::Object(Some(out))))
}

pub(crate) fn native_class_cast(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Simplified: just return the object (no type checking)
    let obj = args.get(1).copied().unwrap_or(Value::Object(None));
    Ok(Some(obj))
}

/// Native override for `java.lang.Class.getClassLoader()`.
///
/// JVM spec §5.3: classes loaded by the bootstrap loader return `null`,
/// otherwise return the defining loader.  In CratonVM:
///
/// - Bootstrap classes (java/*, javax/*, jdk/*, sun/*, com/sun/*) → null.
/// - Anything else (app classpath via `-c`, user-defined hidden classes,
///   synthetic test fixtures, primitive-mirror lookups that arrive here) →
///   the singleton application `ClassLoader` instance.  This must be
///   non-null so callers like `commons-logging`'s
///   `LogFactory.<clinit>` (which does
///   `LogFactory.class.getClassLoader().loadClass(IMPL)`) don't NPE on
///   the next `loadClass` invocation.
///
/// The fix lives in `register_essential_natives` (real-JDK mode); the
/// JDK 25 bytecode for `Class.getClassLoader()` is just
/// `getfield classLoader; areturn`, which would always read `null`
/// (the field is never populated by VM-internal mirror creation).  The
/// native dispatch path in `try_stackless_invoke` / `invoke_or_native`
/// checks the registry first, so this override wins over the bytecode.
pub(crate) fn native_class_get_class_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mirror = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Prefer the authoritative reverse map; fall back to the legacy
    // slot-0 ClassId encoding for synthetic test fixtures.
    let class_id_opt = ctx.class_id_from_mirror(mirror).or_else(|| {
        if let Value::Int(v) = ctx.get_field(mirror, 0) {
            if v > 0 {
                return Some(rustjvm_types::ClassId::new(v as u32));
            }
        }
        None
    });
    let class_id = match class_id_opt {
        Some(cid) => cid,
        None => {
            // No resolvable ClassId — return the app loader so
            // `Class.getClassLoader()` is never null for a non-bootstrap
            // class.  The only path that yields null is the explicit
            // bootstrap-package case below.
            let cl = crate::classloader::get_or_create_app_loader(ctx);
            return Ok(Some(Value::Object(Some(cl))));
        }
    };
    let loader_type = ctx.loader_id_of_class(class_id);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let is_jdk_pkg = class_name.starts_with("java/")
        || class_name.starts_with("javax/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/");
    if loader_type == 0 && is_jdk_pkg {
        // Bootstrap loader → null per JVM spec.
        return Ok(Some(Value::Object(None)));
    }
    if loader_type == 1 {
        // Platform/extension loader — return singleton.
        let cl = crate::classloader::get_or_create_platform_loader(ctx);
        return Ok(Some(Value::Object(Some(cl))));
    }
    // Application class (`-c` classpath), user-defined loader, or a
    // class whose stored loader id is bootstrap but whose name is NOT
    // in a JDK package (= app classpath class registered before the
    // loader-id plumbing was wired up).  Return the singleton app
    // loader so `loadClass` works.
    let cl = crate::classloader::get_or_create_app_loader(ctx);
    Ok(Some(Value::Object(Some(cl))))
}

pub(crate) fn native_class_as_subclass(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Return this class
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

pub(crate) fn native_class_descriptor_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let desc = match name.as_str() {
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        "void" => "V".to_string(),
        _ if name.starts_with('[') => name,
        _ => format!("L{};", name),
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&desc)))))
}

// ---------------------------------------------------------------------------
// T13 — java/lang/Class JDK 25 native method implementations
// ---------------------------------------------------------------------------

/// `java/lang/Class.getDeclaringClass0()Ljava/lang/Class;`
///
/// Returns the Class object of the outer class if this class is an inner class,
/// or null if it is a top-level class. Reads from the InnerClasses attribute.
pub(crate) fn native_class_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    match ctx.declaring_class(class_id) {
        Some(outer_id) => {
            let mirror = ctx.get_class_mirror(outer_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `java/lang/Class.getSimpleBinaryName0()Ljava/lang/String;`
///
/// Returns the simple binary name of this class if it is an inner class
/// (the `inner_name` from the InnerClasses attribute), or null if it is
/// a top-level class or anonymous class.
pub(crate) fn native_class_get_simple_binary_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let class_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };

    // Search inner_classes for an entry where inner_class == this class
    let inner_classes = ctx.inner_classes(class_id);
    for (inner_class, _outer_class, inner_name, _flags) in &inner_classes {
        if inner_class == &class_name {
            if inner_name.is_empty() {
                // Anonymous class — no simple binary name
                return Ok(Some(Value::Object(None)));
            }
            let name_obj = ctx.create_string(inner_name);
            return Ok(Some(Value::Object(Some(name_obj))));
        }
    }

    // Not an inner class — return null
    Ok(Some(Value::Object(None)))
}

/// `java/lang/Class.getEnclosingMethod0()[Ljava/lang/Object;`
///
/// Returns a 3-element Object array `[enclosingClass, methodName, methodDescriptor]`
/// if this class is a local or anonymous class defined inside a method, or null.
pub(crate) fn native_class_get_enclosing_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    match ctx.enclosing_method(class_id) {
        Some((enc_class, method_name, method_desc)) => {
            // Resolve the enclosing class
            let enc_class_id = match ctx.class_id_by_name(&enc_class) {
                Some(id) => id,
                None => return Ok(Some(Value::Object(None))),
            };
            let enc_mirror = ctx.get_class_mirror(enc_class_id);

            // Build 3-element Object array: [Class, String, String]
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 3);
            ctx.set_array_element(arr, 0, Value::Object(Some(enc_mirror)));

            if !method_name.is_empty() {
                let name_obj = ctx.create_string(&method_name);
                ctx.set_array_element(arr, 1, Value::Object(Some(name_obj)));
                let desc_obj = ctx.create_string(&method_desc);
                ctx.set_array_element(arr, 2, Value::Object(Some(desc_obj)));
            }
            // If method_name is empty, slots 1 and 2 stay null (class was defined
            // directly inside an initializer, not a named method).

            Ok(Some(Value::Object(Some(arr))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `java/lang/Class.getGenericSignature0()Ljava/lang/String;`
///
/// Returns the generic signature string from the Signature attribute,
/// or null if no generic signature is present.
pub(crate) fn native_class_get_generic_signature(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    match ctx.class_signature(class_id) {
        Some(sig) => {
            let sig_obj = ctx.create_string(&sig);
            Ok(Some(Value::Object(Some(sig_obj))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `java/lang/Class.getRawAnnotations()[B`
///
/// Returns the raw bytes of the RuntimeVisibleAnnotations attribute,
/// or an empty byte array if no annotations are present.
pub(crate) fn native_class_get_raw_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        // OpenJDK returns null (not empty array) when there are no annotations.
        // AnnotationParser.parseAnnotations checks for null and short-circuits;
        // an empty byte[] would cause BufferUnderflowException -> AnnotationFormatError.
        None => return Ok(Some(Value::Object(None))),
    };

    let bytes = ctx.raw_annotations(class_id);
    if bytes.is_empty() {
        // Match OpenJDK semantics: return null when no RuntimeVisibleAnnotations attribute.
        Ok(Some(Value::Object(None)))
    } else {
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }
}

/// `java/lang/Class.getRawTypeAnnotations()[B`
///
/// Returns the raw bytes of the RuntimeVisibleTypeAnnotations attribute,
/// or null if none are present (matches OpenJDK; an empty byte[] would cause
/// BufferUnderflowException in AnnotationParser).
pub(crate) fn native_class_get_raw_type_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let bytes = ctx.raw_type_annotations(class_id);
    if bytes.is_empty() {
        Ok(Some(Value::Object(None)))
    } else {
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }
}

/// `java/lang/Class.getConstantPool()Ljdk/internal/reflect/ConstantPool;`
///
/// Returns a ConstantPool mirror object. In our implementation, we allocate
/// a synthetic object that holds a reference to the class ID. The JDK uses
/// this for annotation parsing — we return null for now and handle annotation
/// parsing through getRawAnnotations().
pub(crate) fn native_class_get_constant_pool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // Allocate a synthetic ConstantPool object.
    // Field 0 stores the class_id as Int for later lookups.
    let cp_obj = alloc_concurrent_synthetic(ctx, "jdk/internal/reflect/ConstantPool", 2);
    ctx.set_field(cp_obj, 0, Value::Int(class_id.as_u32() as i32));
    // Field 1: store the Class mirror reference for getDeclaringClass()
    ctx.set_field(cp_obj, 1, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cp_obj))))
}

/// `java/lang/Class.getDeclaredClasses0()[Ljava/lang/Class;`
///
/// Returns an array of Class objects for all classes and interfaces that are
/// declared as members of this class. Uses the InnerClasses attribute.
pub(crate) fn native_class_get_declared_classes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let inner_classes = ctx.inner_classes(class_id);

    // Collect inner classes where outer_class == this class
    let mut declared: Vec<ObjectRef> = Vec::new();
    for (inner_class, outer_class, _inner_name, _flags) in &inner_classes {
        if outer_class == &class_name && inner_class != &class_name {
            if let Some(inner_id) = ctx.class_id_by_name(inner_class) {
                declared.push(ctx.get_class_mirror(inner_id));
            }
        }
    }

    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, declared.len());
    for (i, mirror) in declared.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.getNestHost0()Ljava/lang/Class;`
///
/// Returns the nest host of this class. If this class has a NestHost attribute,
/// returns that class; otherwise returns itself (every class is its own nest host).
pub(crate) fn native_class_get_nest_host(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(Some(this)))),
    };

    match ctx.nest_host_name(class_id) {
        Some(host_name) => {
            match ctx.class_id_by_name(&host_name) {
                Some(host_id) => {
                    let mirror = ctx.get_class_mirror(host_id);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                // If the host class can't be resolved, JDK spec says return self
                None => Ok(Some(Value::Object(Some(this)))),
            }
        }
        // No NestHost attribute — this class is its own nest host
        None => Ok(Some(Value::Object(Some(this)))),
    }
}

/// `java/lang/Class.getNestMembers0()[Ljava/lang/Class;`
///
/// Returns the nest members of this class. If this class has a NestMembers
/// attribute (i.e., it is a nest host), returns those classes plus itself.
/// Otherwise returns an array containing just itself.
pub(crate) fn native_class_get_nest_members(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(this)));
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let members = ctx.nest_member_names(class_id);
    if members.is_empty() {
        // Not a nest host — return [self]
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(arr, 0, Value::Object(Some(this)));
        Ok(Some(Value::Object(Some(arr))))
    } else {
        // Nest host — return [self] + resolved members
        let mut mirrors: Vec<ObjectRef> = vec![ctx.get_class_mirror(class_id)];
        for member_name in &members {
            if let Some(member_id) = ctx.class_id_by_name(member_name) {
                if member_id != class_id {
                    mirrors.push(ctx.get_class_mirror(member_id));
                }
            }
        }
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, mirrors.len());
        for (i, mirror) in mirrors.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Object(Some(*mirror)));
        }
        Ok(Some(Value::Object(Some(arr))))
    }
}

/// `java/lang/Class.getRecordComponents0()[Ljava/lang/reflect/RecordComponent;`
///
/// WP2.1: Returns the record components of a record class.
///   * Non-record classes: returns null (per JDK 25 spec).
///   * Record classes: returns array of RecordComponent objects.
///
/// The fields populated on each RecordComponent are:
///   * clazz   — the declaring record Class mirror
///   * name    — the component name (String)
///   * type    — the component type (Class mirror, resolved from descriptor)
///   * accessor — null (computed lazily by the Java side via getDeclaredMethod)
///   * signature, annotations, typeAnnotations — null/zero-length
///
/// Fields are populated via `set_field_by_name` so the impl works with both
/// the real-JDK layout (where Class.getName etc. is pure-Java reading
/// `this.name`) and the synthetic mode (where natives at fixed slots are
/// populated via `register_p60_record`).
pub(crate) fn native_class_get_record_components(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    if !ctx.is_record_class(class_id) {
        // Non-record returns null (matches JDK Class.getRecordComponents0 contract).
        return Ok(Some(Value::Object(None)));
    }

    let components = ctx.record_components(class_id);
    let rc_class_id = ctx
        .ensure_class_initialized("java/lang/reflect/RecordComponent")
        .unwrap_or(rustjvm_types::ClassId::new(0));

    // Allocate with the JDK-layout total field count, falling back to a
    // 3-slot floor for synthetic mode where the class is unknown.
    let jdk_layout_fields = ctx.class_num_total_fields(rc_class_id);
    let num_fields = if jdk_layout_fields >= 3 { jdk_layout_fields } else { 3 };

    // Resolve declared methods once so we can build the per-component
    // accessor link without re-querying the class manager per component.
    // For `record Foo(int a, String b)`, the Record attribute lists
    // (a, I) and (b, Ljava/lang/String;); the synthesized accessors are
    // `int a()` (descriptor `()I`) and `String b()` (descriptor
    // `()Ljava/lang/String;`) — i.e. zero-arg, return type matches the
    // component descriptor.
    let declared = ctx.declared_methods(class_id);

    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, components.len());
    for (i, (name, descriptor)) in components.iter().enumerate() {
        let rc_obj = ctx.alloc_object(rc_class_id, num_fields);
        let name_str = ctx.create_string(name);
        let type_mirror = descriptor_to_class_mirror(ctx, descriptor);

        // WP2.1 FAIL-11 fix: build the accessor Method link.
        // JVMS 4.7.30 / JLS 8.10.3: a record component named `n` of type
        // `T` is paired with an accessor method named `n` taking no args
        // and returning `T`.  Match against `declared_methods` by name +
        // exact descriptor `()<component_descriptor>` so we don't pick up
        // a same-named overload that the user happens to have declared.
        let expected_desc = format!("(){}", descriptor);
        let accessor_obj = declared
            .iter()
            .find(|m| m.name == *name && m.descriptor == expected_desc)
            .map(|m| create_method_object(ctx, m));
        let accessor_value = match accessor_obj {
            Some(obj) => Value::Object(Some(obj)),
            None => Value::Object(None),
        };

        // Populate by-name (works for both real-JDK layout and synthetic).
        ctx.set_field_by_name(rc_obj, "clazz", Value::Object(Some(this)));
        ctx.set_field_by_name(rc_obj, "name", Value::Object(Some(name_str)));
        ctx.set_field_by_name(rc_obj, "type", Value::Object(Some(type_mirror)));
        ctx.set_field_by_name(rc_obj, "accessor", accessor_value);
        ctx.set_field_by_name(rc_obj, "signature", Value::Object(None));
        ctx.set_field_by_name(rc_obj, "annotations", Value::Object(None));
        ctx.set_field_by_name(rc_obj, "typeAnnotations", Value::Object(None));

        // Belt-and-suspenders: also populate the synthetic fixed-slot layout
        // (slot 0=name, slot 1=type, slot 2=declaringRecord) used by
        // `lang_misc::register_p60_record`. Real-JDK layout reads via getfield
        // so this duplication is harmless when the class has its own field
        // mapping that overrides.
        if jdk_layout_fields == 0 {
            ctx.set_field(rc_obj, 0, Value::Object(Some(name_str)));
            ctx.set_field(rc_obj, 1, Value::Object(Some(type_mirror)));
            ctx.set_field(rc_obj, 2, Value::Object(Some(this)));
        }

        ctx.set_array_element(arr, i, Value::Object(Some(rc_obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.getPermittedSubclasses0()[Ljava/lang/Class;`
///
/// Returns the permitted subclasses of a sealed class, or null if not sealed.
pub(crate) fn native_class_get_permitted_subclasses(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    if !ctx.is_sealed_class(class_id) {
        return Ok(Some(Value::Object(None)));
    }

    let subs = ctx.permitted_subclasses(class_id);
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, subs.len());
    for (i, sub_name) in subs.iter().enumerate() {
        if let Some(sub_id) = ctx.class_id_by_name(sub_name) {
            let mirror = ctx.get_class_mirror(sub_id);
            ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.getAnnotatedSuperclass()Ljava/lang/reflect/AnnotatedType;`
///
/// WP2.1-class-modern: returns an `AnnotatedType` for the direct superclass
/// of this Class. Synthetic best-effort impl: builds a minimal
/// `AnnotatedType` whose backing `Type` is the superclass `Class` mirror,
/// with no type-annotations attached. Returns null for `Object`, primitive
/// types, void, array types, and interfaces — matching the JDK contract.
///
/// The returned object is a synthetic 2-field stand-in:
///   * slot 0: backing `Type` (the superclass `Class` mirror)
///   * slot 1: empty `Annotation[]` (placeholder for future RUNTIME
///     type-annotation wiring)
///
/// Frameworks that probe `Class.getAnnotatedSuperclass()` usually only
/// need it to be non-null + not throw (Hibernate's
/// `ReflectionUtil.scanForAnnotatedTypes`); the synthetic backing is
/// sufficient to keep their `<clinit>` chain alive.
pub(crate) fn native_class_get_annotated_superclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // Match JDK contract: null for Object, primitives, void, array, interfaces.
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    if name == "java/lang/Object" || name.starts_with('[') || name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }

    // Resolve the direct superclass via `NativeContext::superclass_of`.
    let super_mirror = match ctx.superclass_of(class_id) {
        Some(sid) => ctx.get_class_mirror(sid),
        None => return Ok(Some(Value::Object(None))),
    };

    Ok(Some(Value::Object(Some(make_annotated_type(ctx, super_mirror)))))
}

/// `java/lang/Class.getAnnotatedInterfaces()[Ljava/lang/reflect/AnnotatedType;`
///
/// WP2.1-class-modern: returns an `AnnotatedType[]` mirroring the
/// `getInterfaces()` array. Each element is a synthetic `AnnotatedType`
/// wrapping the corresponding interface `Class` mirror — see
/// [`make_annotated_type`] for the layout.
///
/// Always returns a non-null (possibly zero-length) array — matching the
/// JDK contract. Frameworks (ByteBuddy, JMX OpenMBean introspector) rely
/// on the non-null guarantee; throwing or returning null here breaks
/// `MBeanIntrospector.getMethods` recursion.
pub(crate) fn native_class_get_annotated_interfaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let iface_ids = ctx.class_interfaces(class_id);
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), iface_ids.len());
    for (i, iface_id) in iface_ids.iter().enumerate() {
        let iface_mirror = ctx.get_class_mirror(*iface_id);
        let at = make_annotated_type(ctx, iface_mirror);
        ctx.set_array_element(arr, i, Value::Object(Some(at)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Build a minimal synthetic `AnnotatedType` object wrapping a `Type`
/// (typically a `Class` mirror).
///
/// Layout (2 fields, by-name + slot-fallback):
///   * `type` — the wrapped `Type` (slot 0)
///   * `annotations` — empty `Annotation[]` (slot 1)
///
/// `AnnotatedType` is an interface in the JDK; its concrete impl class is
/// `sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedTypeBaseImpl` /
/// `AnnotatedTypeImpl`. We allocate against `java/lang/reflect/AnnotatedType`
/// — the dispatch path treats this as a synthetic-stub instance.
/// `getType()` reads slot 0 ; downstream frameworks only need that
/// accessor + non-null-ness.
fn make_annotated_type(
    ctx: &mut dyn NativeContext,
    backing_type: rustjvm_types::ObjectRef,
) -> rustjvm_types::ObjectRef {
    // Try the impl class first (real-JDK layout); fall back to the
    // interface name (synthetic-mode placeholder).
    let cid = ctx
        .ensure_class_initialized("sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl")
        .or_else(|_| ctx.ensure_class_initialized("java/lang/reflect/AnnotatedType"))
        .unwrap_or(rustjvm_types::ClassId::new(0));

    let layout_fields = ctx.class_num_total_fields(cid);
    let num_fields = if layout_fields >= 2 { layout_fields } else { 2 };
    let obj = ctx.alloc_object(cid, num_fields);

    // empty Annotation[] for `annotations`
    let empty_anns = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);

    ctx.set_field_by_name(obj, "type", Value::Object(Some(backing_type)));
    ctx.set_field_by_name(obj, "annotations", Value::Object(Some(empty_anns)));
    if layout_fields == 0 {
        ctx.set_field(obj, 0, Value::Object(Some(backing_type)));
        ctx.set_field(obj, 1, Value::Object(Some(empty_anns)));
    }
    obj
}

/// `java/lang/Class.getClassFileVersion0()I`
///
/// Returns the class file version number. The JDK encodes this as
/// `(major << 16) | minor`, but many callers just want the major version.
/// We return the major version number (e.g., 65 for Java 21, 69 for Java 25).
pub(crate) fn native_class_get_class_file_version(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(65))), // Default: Java 21
    };

    let version = ctx.class_file_version(class_id);
    Ok(Some(Value::Int(version as i32)))
}

// ---------------------------------------------------------------------------
// T19.N1: java/lang/Class security-related natives
// ---------------------------------------------------------------------------
//
// Three natives that expose a class's `CodeSource` (installed by the class
// loader at class-define time) to reflective callers:
//
//   * `getProtectionDomain0()` returns a lightweight `ProtectionDomain`
//     whose `CodeSource` carries the class's load URL and any signer
//     certificates.  Bootstrap / synthetic classes (empty or `class:`
//     code-base) return null — matching HotSpot for boot classes.
//
//   * `getSigners()` returns the raw signer-certificate blocks as
//     `byte[]` elements of an `Object[]`, or null for unsigned classes.
//     Real JDK returns `Certificate[]` — we approximate with byte arrays
//     since our synthetic-JDK surface doesn't fully implement
//     `java.security.cert.Certificate`.
//
//   * `setSigners([Ljava/lang/Object;)V` is a documented no-op at the
//     moment (we do not track mutable per-class signers).  A
//     `tracing::debug!` fires so any runtime dependency on the setter
//     surfaces in logs rather than silently corrupting state.

/// `java/lang/Class.getProtectionDomain0()Ljava/security/ProtectionDomain;`
///
/// Returns the `ProtectionDomain` for this class.
///
/// * For classes loaded from the bootstrap loader (no CodeSource URL, or a
///   synthetic `class:...` placeholder URL), returns null — matching HotSpot
///   where the boot loader's PD is `null`.
/// * For classes with a real CodeSource URL (typically `file:/...` from a
///   JAR or directory classpath entry), allocates a minimal-viable
///   `java/security/ProtectionDomain` with a 1-field `CodeSource` child
///   holding the URL and any signer certs.  Permissions and class loader
///   fields are left null which the JDK spec treats as "all permissions,
///   bootstrap loader" — the safest default for a non-enforcing policy.
pub(crate) fn native_class_get_protection_domain0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let mut code_base = ctx.class_code_base(class_id).unwrap_or_default();
    // `Class.code_source` can be missing on some mirror edges; Spring Boot's
    // `Launcher.createArchive` needs a real `file:` URL. Mirror
    // `Class.getProtectionDomain` (lib.rs): fall back to the first
    // `java.class.path` entry (the executable JAR for `java -jar`).
    if code_base.is_empty() || code_base.starts_with("class:") {
        code_base = ctx
            .get_system_property("java.class.path")
            .and_then(|cp| {
                let sep = if cfg!(windows) { ';' } else { ':' };
                cp.split(sep)
                    .next()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
    }
    if code_base.is_empty() || code_base.starts_with("class:") {
        return Ok(Some(Value::Object(None)));
    }

    // Nested JAR `code_source` is `jar:file:/outer.jar!/inner.jar`. Spring's
    // `Archive.create(File)` requires the outer filesystem path only.
    let code_base = if let Some(rest) = code_base.strip_prefix("jar:file:") {
        let outer = rest.split('!').next().unwrap_or(rest);
        let outer = outer.trim_start_matches('/');
        format!("file:/{}", outer)
    } else {
        code_base
    };

    // HotSpot-style path inside the `file:` URL (always `/C:/…` on Windows).
    let raw_path = code_base
        .strip_prefix("file:")
        .map(str::to_string)
        .unwrap_or_else(|| code_base.clone());
    let fwd = raw_path.replace('\\', "/");
    let path = if fwd.len() >= 2 && fwd.as_bytes()[1] == b':' {
        format!("/{fwd}")
    } else {
        fwd
    };

    // Build CodeSource(url=location, certs=[]) — signer certs are attached
    // as raw byte[] blocks so the reflective surface survives
    // `getCodeSource().getCertificates()` without requiring a full
    // `java.security.cert.Certificate` implementation.
    //
    // `java.net.URL` uses a 13-field JDK layout where slot 5 is `authority`.
    // Do **not** call `url_parse` here: our 6-field `URL_FIELD_FULL` index
    // collides with `authority`, corrupting `URL.toString()` / `toURI()` for
    // Spring Boot's launcher.
    let url_obj = alloc_concurrent_synthetic(ctx, "java/net/URL", 13);
    let path_obj = ctx.create_string(&path);
    let proto_obj = ctx.create_string("file");
    let host_obj = ctx.create_string("");
    ctx.set_field(url_obj, 0, Value::Object(Some(proto_obj)));
    ctx.set_field(url_obj, 1, Value::Object(Some(host_obj)));
    ctx.set_field(url_obj, 2, Value::Int(-1));
    ctx.set_field(url_obj, 3, Value::Object(Some(path_obj)));
    ctx.set_field(url_obj, 5, Value::Object(None));
    ctx.set_field(url_obj, 6, Value::Object(Some(path_obj)));
    let cs_cid = ctx
        .ensure_class_initialized("java/security/CodeSource")
        .unwrap_or(rustjvm_types::ClassId::new(0));
    let cs_num_fields = ctx.class_num_total_fields(cs_cid).max(2);
    let cs = ctx.alloc_object(cs_cid, cs_num_fields);
    // Slot-based writes cover the synthetic layout; name-based writes
    // cover the real-JDK-loaded layout. At least one lands on the right
    // slot for each mode.
    ctx.set_field(cs, 0, Value::Object(Some(url_obj)));
    ctx.set_field_by_name(cs, "location", Value::Object(Some(url_obj)));

    // Attach signer certs as fresh byte[] copies on slot 1.
    let certs = ctx.class_code_source_certs(class_id);
    if !certs.is_empty() {
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, certs.len());
        for (i, cert) in certs.iter().enumerate() {
            let cert_arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, cert.len());
            for (j, &b) in cert.iter().enumerate() {
                ctx.set_array_element(cert_arr, j, Value::Int(b as i8 as i32));
            }
            ctx.set_array_element(arr, i, Value::Object(Some(cert_arr)));
        }
        ctx.set_field(cs, 1, Value::Object(Some(arr)));
        ctx.set_field_by_name(cs, "certs", Value::Object(Some(arr)));
    }

    // Build ProtectionDomain(codesource=cs, permissions=null, classloader=null,
    // principals=empty).  Null permissions = "all permissions" per JDK default.
    let pd_cid = ctx
        .ensure_class_initialized("java/security/ProtectionDomain")
        .unwrap_or(rustjvm_types::ClassId::new(0));
    let pd_num_fields = ctx.class_num_total_fields(pd_cid).max(4);
    let pd = ctx.alloc_object(pd_cid, pd_num_fields);
    ctx.set_field(pd, 0, Value::Object(Some(cs)));
    ctx.set_field(pd, 1, Value::Object(None));
    ctx.set_field(pd, 2, Value::Object(None));
    let empty_principals = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    ctx.set_field(pd, 3, Value::Object(Some(empty_principals)));
    ctx.set_field_by_name(pd, "codesource", Value::Object(Some(cs)));
    ctx.set_field_by_name(pd, "permissions", Value::Object(None));
    ctx.set_field_by_name(pd, "classloader", Value::Object(None));
    ctx.set_field_by_name(pd, "principals", Value::Object(Some(empty_principals)));

    Ok(Some(Value::Object(Some(pd))))
}

/// `java/lang/Class.getSigners()[Ljava/lang/Object;`
///
/// Returns the signer certificates of this class as an `Object[]` whose
/// elements are `byte[]` copies of the raw PKCS#7 signer blocks from the
/// class's `CodeSource.certificates`.  Returns null for unsigned classes
/// (including every bootstrap class, since jimage classes have no signers).
///
/// Real JDK returns `Certificate[]`; we approximate with `Object[]` + `byte[]`
/// elements because our synthetic JDK surface does not fully implement
/// `java.security.cert.Certificate`.  Callers that merely check
/// `signers != null` or use the bytes for digest-match work correctly;
/// callers that cast the elements to `Certificate` in synthetic mode will
/// see `ClassCastException`, which is the correct failure mode for
/// a class not yet wired up.
pub(crate) fn native_class_get_signers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let certs = ctx.class_code_source_certs(class_id);
    if certs.is_empty() {
        return Ok(Some(Value::Object(None)));
    }

    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, certs.len());
    for (i, cert) in certs.iter().enumerate() {
        // Fresh byte[] copy — never leak the internal `CodeSource.certificates`
        // Vec<Vec<u8>> pointer to the caller.
        let cert_arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, cert.len());
        for (j, &b) in cert.iter().enumerate() {
            ctx.set_array_element(cert_arr, j, Value::Int(b as i8 as i32));
        }
        ctx.set_array_element(arr, i, Value::Object(Some(cert_arr)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.setSigners([Ljava/lang/Object;)V`
///
/// Documented no-op.  Real HotSpot stores the passed signers on the
/// `Class` metadata where `getSigners()` later returns them.  We do not
/// currently track mutable per-class signers (the `CodeSource` installed
/// at class-define time is immutable in our class store), so this setter
/// silently discards its argument.
///
/// A `tracing::debug!` fires when `setSigners` is called so any runtime
/// reliance on the post-define-time setter surfaces in logs rather than
/// silently corrupting security policy enforcement.
///
/// Per the JDK spec `setSigners` is effectively package-private to
/// `java.lang.ClassLoader`; arbitrary user code calling it is either a
/// test harness or a misbehaving agent.  We do not enforce a caller-class
/// check because that would require walking the stack for every call
/// (this native is rarely invoked after class loading completes).
pub(crate) fn native_class_set_signers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = mirror_class_name(ctx, this).unwrap_or_else(|| "<unknown>".to_string());
    let signer_count = match args.get(1) {
        Some(Value::Object(Some(arr))) => ctx.array_length(*arr),
        _ => 0,
    };
    tracing::debug!(
        target: "rustjvm_native_builtins::lang_class",
        class = %class_name,
        signer_count = signer_count,
        "Class.setSigners called — documented no-op (signers not mutable post-define)"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use rustjvm_native_api::NativeContext;

    /// Helper: create a Class mirror object with the given class_id and name.
    fn make_class_mirror(ctx: &mut crate::test_utils::MockNativeContext, class_id: u32, name: &str) -> ObjectRef {
        let name_obj = ctx.create_string(name);
        let mirror = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(mirror, 0, Value::Int(class_id as i32));
        ctx.set_field(mirror, 1, Value::Object(Some(name_obj)));
        mirror
    }

    // -----------------------------------------------------------------------
    // parse_descriptor_param_and_return (pure function)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_descriptor_empty_params_void() {
        let (params, ret) = parse_descriptor_param_and_return("()V");
        assert!(params.is_empty());
        assert_eq!(ret, "V");
    }

    #[test]
    fn parse_descriptor_int_param_int_return() {
        let (params, ret) = parse_descriptor_param_and_return("(I)I");
        assert_eq!(params, vec!["I"]);
        assert_eq!(ret, "I");
    }

    #[test]
    fn parse_descriptor_mixed_params() {
        let (params, ret) =
            parse_descriptor_param_and_return("(ILjava/lang/String;D)V");
        assert_eq!(params, vec!["I", "Ljava/lang/String;", "D"]);
        assert_eq!(ret, "V");
    }

    #[test]
    fn parse_descriptor_array_param() {
        let (params, ret) = parse_descriptor_param_and_return("([I[Ljava/lang/Object;)Z");
        assert_eq!(params, vec!["[I", "[Ljava/lang/Object;"]);
        assert_eq!(ret, "Z");
    }

    #[test]
    fn parse_descriptor_object_return() {
        let (params, ret) =
            parse_descriptor_param_and_return("()Ljava/lang/String;");
        assert!(params.is_empty());
        assert_eq!(ret, "Ljava/lang/String;");
    }

    // -----------------------------------------------------------------------
    // Class.getName
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_name_basic() {
        let mut ctx = mock_ctx();
        // Ensure the class is known so class_name_of_id returns a name
        let cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "java/lang/Object");
        let r = native_class_get_name(&mut ctx, &[Value::Object(Some(mirror))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected string Object, got {other:?}"),
        };
        // getName converts / to .
        assert_eq!(ctx.read_string(obj).unwrap(), "java.lang.Object");
    }

    #[test]
    fn class_get_name_null_returns_null() {
        let mut ctx = mock_ctx();
        let r = native_class_get_name(&mut ctx, &[Value::Object(None)]);
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    // -----------------------------------------------------------------------
    // C14: Class.getEnumConstants{,Shared} — must read `$VALUES`, not
    // return a stub empty array (which broke `EnumMap.<init>` →
    // `StreamOpFlag.<clinit>`).
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_enum_constants_non_enum_returns_null() {
        let mut ctx = mock_ctx();
        // Class without ACC_ENUM should yield null (matches JDK spec).
        let cid = ctx.ensure_class_initialized("java/lang/String").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "java/lang/String");
        let r = native_class_get_enum_constants(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn class_get_enum_constants_returns_populated_timeunit_array() {
        // Simulates `Class.getEnumConstantsShared(TimeUnit.class)` —
        // asserts that the native sees ACC_ENUM set, reads `$VALUES`,
        // and returns a fresh Object[] with 7 non-null elements.
        use rustjvm_types::ArrayElementType;
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("java/util/concurrent/TimeUnit").unwrap();
        // Flag the class as an enum (ACC_ENUM = 0x4000).
        unsafe { (*ctx.class_flags_override.get()).insert(cid.as_u32(), 0x4000); }
        // Populate a synthetic `$VALUES` array with 7 non-null entries —
        // one per TimeUnit constant (NANOSECONDS .. DAYS).
        let values_arr = ctx.new_array(ArrayElementType::Reference, 7);
        for i in 0..7 {
            let elem = make_class_mirror(&mut ctx, cid.as_u32(), &format!("TimeUnit{i}"));
            ctx.set_array_element(values_arr, i, Value::Object(Some(elem)));
        }
        unsafe { (*ctx.enum_values_override.get()).insert(cid.as_u32(), values_arr); }

        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "java/util/concurrent/TimeUnit");
        let r = native_class_get_enum_constants(&mut ctx, &[Value::Object(Some(mirror))]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null Object[], got {other:?}"),
        };
        assert_eq!(ctx.array_length(arr), 7, "TimeUnit has 7 enum constants");
        for i in 0..7 {
            match ctx.get_array_element(arr, i) {
                Value::Object(Some(_)) => {}
                other => panic!("element {i} is null/wrong: {other:?}"),
            }
        }
        // Returned array must be a copy, not the backing $VALUES itself,
        // so callers can mutate (e.g. clone()) without aliasing.
        assert_ne!(arr.as_ptr() as usize, values_arr.as_ptr() as usize);
    }

    // -----------------------------------------------------------------------
    // Class.isArray
    // -----------------------------------------------------------------------

    #[test]
    fn class_is_array_true() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "[Ljava/lang/String;");
        let r = native_class_is_array(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn class_is_array_false() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "java/lang/String");
        let r = native_class_is_array(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // Class.isPrimitive
    // -----------------------------------------------------------------------

    #[test]
    fn class_is_primitive_int() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "int");
        let r = native_class_is_primitive(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn class_is_primitive_object() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "java/lang/Object");
        let r = native_class_is_primitive(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn class_is_primitive_boolean() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "boolean");
        let r = native_class_is_primitive(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    // -----------------------------------------------------------------------
    // Class.getSimpleName
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_simple_name_basic() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "java/lang/String");
        let r = native_class_get_simple_name(&mut ctx, &[Value::Object(Some(mirror))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "String");
    }

    #[test]
    fn class_get_simple_name_inner_class() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "java/util/Map$Entry");
        let r = native_class_get_simple_name(&mut ctx, &[Value::Object(Some(mirror))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "Entry");
    }

    // -----------------------------------------------------------------------
    // Class.descriptorString
    // -----------------------------------------------------------------------

    #[test]
    fn class_descriptor_string_int() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "int");
        let r = native_class_descriptor_string(&mut ctx, &[Value::Object(Some(mirror))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "I");
    }

    #[test]
    fn class_descriptor_string_object() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "java/lang/Object");
        let r = native_class_descriptor_string(&mut ctx, &[Value::Object(Some(mirror))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "Ljava/lang/Object;");
    }

    #[test]
    fn class_descriptor_string_array() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "[I");
        let r = native_class_descriptor_string(&mut ctx, &[Value::Object(Some(mirror))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "[I");
    }

    // -----------------------------------------------------------------------
    // Class.isRecord, Class.isSealed (mock always returns false)
    // -----------------------------------------------------------------------

    #[test]
    fn class_is_record_false() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_is_record(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn class_is_sealed_false() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_is_sealed(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // Class.asSubclass (returns this)
    // -----------------------------------------------------------------------

    #[test]
    fn class_as_subclass_returns_this() {
        let mut ctx = mock_ctx();
        let mirror = make_class_mirror(&mut ctx, 0, "java/lang/Object");
        let r = native_class_as_subclass(&mut ctx, &[Value::Object(Some(mirror))]);
        assert_eq!(r.unwrap(), Some(Value::Object(Some(mirror))));
    }

    // -----------------------------------------------------------------------
    // descriptor_to_class_mirror (needs context)
    // -----------------------------------------------------------------------

    #[test]
    fn descriptor_to_class_mirror_primitive() {
        let mut ctx = mock_ctx();
        let mirror = descriptor_to_class_mirror(&mut ctx, "I");
        // Should be a primitive class mirror with name "int"
        let name = mirror_class_name(&ctx, mirror);
        assert_eq!(name.as_deref(), Some("int"));
    }

    #[test]
    fn descriptor_to_class_mirror_array() {
        let mut ctx = mock_ctx();
        let mirror = descriptor_to_class_mirror(&mut ctx, "[I");
        let name = mirror_class_name(&ctx, mirror);
        assert_eq!(name.as_deref(), Some("[I"));
    }

    #[test]
    fn descriptor_to_class_mirror_void() {
        let mut ctx = mock_ctx();
        let mirror = descriptor_to_class_mirror(&mut ctx, "V");
        let name = mirror_class_name(&ctx, mirror);
        assert_eq!(name.as_deref(), Some("void"));
    }

    // -----------------------------------------------------------------------
    // T13 — getDeclaringClass0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_declaring_class_returns_null_for_top_level() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_declaring_class(&mut ctx, &[Value::Object(Some(mirror))]);
        // Default mock returns None for declaring_class → null
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn class_get_declaring_class_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_declaring_class(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err()); // obj_arg fails on null
    }

    // -----------------------------------------------------------------------
    // T13 — getSimpleBinaryName0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_simple_binary_name_top_level_returns_null() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_simple_binary_name(&mut ctx, &[Value::Object(Some(mirror))]);
        // No inner_classes data → null
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn class_get_simple_binary_name_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_simple_binary_name(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // T13 — getEnclosingMethod0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_enclosing_method_returns_null_for_top_level() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_enclosing_method(&mut ctx, &[Value::Object(Some(mirror))]);
        // Default mock returns None for enclosing_method → null
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn class_get_enclosing_method_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_enclosing_method(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // T13 — getGenericSignature0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_generic_signature_returns_null_when_absent() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_generic_signature(&mut ctx, &[Value::Object(Some(mirror))]);
        // Default mock returns None for class_signature → null
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn class_get_generic_signature_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_generic_signature(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // T13 — getRawAnnotations
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_raw_annotations_empty() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_raw_annotations(&mut ctx, &[Value::Object(Some(mirror))]);
        // Default mock returns empty Vec → null (matches OpenJDK; empty byte[] would
        // crash AnnotationParser.parseAnnotations with BufferUnderflowException).
        match r.unwrap() {
            Some(Value::Object(None)) => (),
            other => panic!("expected null, got {other:?}"),
        }
    }

    #[test]
    fn class_get_raw_annotations_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_raw_annotations(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // T13 — getRawTypeAnnotations
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_raw_type_annotations_empty() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_raw_type_annotations(&mut ctx, &[Value::Object(Some(mirror))]);
        // Empty -> null (matches OpenJDK).
        match r.unwrap() {
            Some(Value::Object(None)) => (),
            other => panic!("expected null, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // T13 — getConstantPool
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_constant_pool_returns_object() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_constant_pool(&mut ctx, &[Value::Object(Some(mirror))]);
        let cp = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected ConstantPool object, got {other:?}"),
        };
        // Field 0 should hold the class_id
        assert_eq!(ctx.get_field(cp, 0), Value::Int(cid.as_u32() as i32));
        // Field 1 should hold the Class mirror
        assert_eq!(ctx.get_field(cp, 1), Value::Object(Some(mirror)));
    }

    #[test]
    fn class_get_constant_pool_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_constant_pool(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // T13 — getDeclaredClasses0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_declared_classes_empty() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_declared_classes(&mut ctx, &[Value::Object(Some(mirror))]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected array, got {other:?}"),
        };
        // No inner_classes data → empty array
        assert_eq!(ctx.array_length(arr), 0);
    }

    // -----------------------------------------------------------------------
    // T13 — getNestHost0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_nest_host_returns_self_when_no_attribute() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_nest_host(&mut ctx, &[Value::Object(Some(mirror))]);
        // No nest host attribute → returns self
        let result = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(result, mirror);
    }

    // -----------------------------------------------------------------------
    // T13 — getNestMembers0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_nest_members_returns_self_when_no_attribute() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_nest_members(&mut ctx, &[Value::Object(Some(mirror))]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected array, got {other:?}"),
        };
        // Not a nest host → returns [self]
        assert_eq!(ctx.array_length(arr), 1);
        let first = ctx.get_array_element(arr, 0);
        assert_eq!(first, Value::Object(Some(mirror)));
    }

    // -----------------------------------------------------------------------
    // T13 — getPermittedSubclasses0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_permitted_subclasses_returns_null_when_not_sealed() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_permitted_subclasses(&mut ctx, &[Value::Object(Some(mirror))]);
        // Not sealed → null
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    // -----------------------------------------------------------------------
    // T13 — getClassFileVersion0
    // -----------------------------------------------------------------------

    #[test]
    fn class_get_class_file_version_default() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Foo").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Foo");
        let r = native_class_get_class_file_version(&mut ctx, &[Value::Object(Some(mirror))]);
        // Default mock returns 65 (Java 21)
        assert_eq!(r.unwrap(), Some(Value::Int(65)));
    }

    #[test]
    fn class_get_class_file_version_null_mirror() {
        let mut ctx = mock_ctx();
        let r = native_class_get_class_file_version(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // Phase C hardening — wrapper matching, widening, strict coercion
    // -----------------------------------------------------------------------

    #[test]
    fn wrapper_matches_primitive_exact() {
        assert!(wrapper_matches_primitive("java/lang/Integer", "I"));
        assert!(wrapper_matches_primitive("java/lang/Long", "J"));
        assert!(wrapper_matches_primitive("java/lang/Float", "F"));
        assert!(wrapper_matches_primitive("java/lang/Double", "D"));
        assert!(wrapper_matches_primitive("java/lang/Boolean", "Z"));
        assert!(wrapper_matches_primitive("java/lang/Byte", "B"));
        assert!(wrapper_matches_primitive("java/lang/Short", "S"));
        assert!(wrapper_matches_primitive("java/lang/Character", "C"));
    }

    #[test]
    fn wrapper_matches_primitive_mismatch() {
        assert!(!wrapper_matches_primitive("java/lang/Integer", "J"));
        assert!(!wrapper_matches_primitive("java/lang/String", "I"));
        assert!(!wrapper_matches_primitive("java/lang/Long", "I"));
    }

    #[test]
    fn widening_allowed_spec_table() {
        // JLS §5.1.2 widening conversions
        assert!(widening_allowed("B", "S"));
        assert!(widening_allowed("B", "I"));
        assert!(widening_allowed("B", "J"));
        assert!(widening_allowed("B", "F"));
        assert!(widening_allowed("B", "D"));
        assert!(widening_allowed("S", "I"));
        assert!(widening_allowed("S", "J"));
        assert!(widening_allowed("C", "I"));
        assert!(widening_allowed("C", "D"));
        assert!(widening_allowed("I", "J"));
        assert!(widening_allowed("I", "D"));
        assert!(widening_allowed("J", "F"));
        assert!(widening_allowed("F", "D"));
        // Identity is trivially allowed.
        assert!(widening_allowed("I", "I"));
    }

    #[test]
    fn widening_disallowed_narrowing() {
        // Narrowing conversions require an explicit cast; they are NOT
        // implicit widenings.
        assert!(!widening_allowed("J", "I"));
        assert!(!widening_allowed("D", "F"));
        assert!(!widening_allowed("I", "B"));
        assert!(!widening_allowed("I", "S"));
        assert!(!widening_allowed("I", "C"));
        // boolean does not widen to anything else.
        assert!(!widening_allowed("Z", "I"));
        assert!(!widening_allowed("I", "Z"));
        // short does not widen to char per JLS §5.1.2 (explicit cast only).
        assert!(!widening_allowed("S", "C"));
        assert!(!widening_allowed("C", "S"));
    }

    #[test]
    fn widen_primitive_value_int_to_long() {
        let v = widen_primitive_value(Value::Int(42), "I", "J").unwrap();
        assert_eq!(v, Value::Long(42));
    }

    #[test]
    fn widen_primitive_value_int_to_double() {
        let v = widen_primitive_value(Value::Int(7), "I", "D").unwrap();
        match v {
            Value::Double(d) => assert!((d - 7.0).abs() < f64::EPSILON),
            other => panic!("expected Double, got {other:?}"),
        }
    }

    #[test]
    fn widen_primitive_value_disallowed_returns_none() {
        assert!(widen_primitive_value(Value::Long(1), "J", "I").is_none());
        assert!(widen_primitive_value(Value::Double(1.0), "D", "F").is_none());
    }

    #[test]
    fn wrapper_to_prim_desc_known_and_unknown() {
        assert_eq!(wrapper_to_prim_desc("java/lang/Integer"), Some("I"));
        assert_eq!(wrapper_to_prim_desc("java/lang/Boolean"), Some("Z"));
        assert_eq!(wrapper_to_prim_desc("java/lang/String"), None);
    }

    #[test]
    fn coerce_arg_strict_primitive_value_stays() {
        let mut ctx = mock_ctx();
        let v = coerce_arg_strict(&ctx, Value::Int(7), "I", "test").unwrap();
        assert_eq!(v, Value::Int(7));
        // int → long widening is allowed implicitly.
        let v = coerce_arg_strict(&ctx, Value::Int(7), "J", "test").unwrap();
        assert_eq!(v, Value::Long(7));
        // long → int is narrowing and must fail.
        let r = coerce_arg_strict(&ctx, Value::Long(7), "I", "test");
        assert!(r.is_err(), "narrowing long → int should be rejected");
        drop(ctx);
    }

    #[test]
    fn coerce_arg_strict_null_to_primitive_errors() {
        let ctx = mock_ctx();
        let r = coerce_arg_strict(&ctx, Value::Object(None), "I", "test");
        assert!(r.is_err(), "null cannot be coerced to primitive");
    }

    #[test]
    fn coerce_arg_strict_reference_passes_through() {
        let mut ctx = mock_ctx();
        let obj = ctx.alloc_object(ClassId::new(0), 0);
        let v = coerce_arg_strict(
            &ctx,
            Value::Object(Some(obj)),
            "Ljava/lang/Object;",
            "test",
        )
        .unwrap();
        assert_eq!(v, Value::Object(Some(obj)));
        // null is legal for a reference type.
        let v = coerce_arg_strict(&ctx, Value::Object(None), "Ljava/lang/String;", "test").unwrap();
        assert_eq!(v, Value::Object(None));
    }

    #[test]
    fn coerce_arg_strict_primitive_to_reference_errors() {
        let ctx = mock_ctx();
        // Passing an int where a reference is expected is an error.
        let r = coerce_arg_strict(&ctx, Value::Int(1), "Ljava/lang/String;", "test");
        assert!(r.is_err());
    }

    #[test]
    fn illegal_arg_exc_carries_message() {
        use rustjvm_types::error::{MethodCallFailed, RuntimeError, VmError};
        let err = illegal_arg_exc("bad".to_string());
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            )) => assert_eq!(message, "bad"),
            other => panic!("expected IllegalArgumentException, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase C — typed Field setters narrow via storage-size mask
    // -----------------------------------------------------------------------

    /// Helper: allocate a Field mirror object that references a storage
    /// field on an arbitrary target object. Used to drive the typed
    /// getters/setters without a full class-loader path.
    fn make_field_mirror(
        ctx: &mut crate::test_utils::MockNativeContext,
        name: &str,
        descriptor: &str,
        modifiers: i32,
        slot_index: i32,
    ) -> ObjectRef {
        // Allocate with room for the synthetic layout (slots 0..=6) plus
        // the RustJVM extra-metadata slots (descriptor, rj_slot,
        // accessible) that the production readers now consult.
        let obj = ctx.alloc_object(
            ClassId::new(0),
            FIELD_NUM_FIELDS + FIELD_EXTRA_SLOTS,
        );
        // field 0: declaring class mirror (class_id = 1 arbitrary)
        let mirror = make_class_mirror(ctx, 1, "rustjvm/test/Fixture");
        ctx.set_field(obj, 0, Value::Object(Some(mirror)));
        let name_s = ctx.create_string(name);
        ctx.set_field(obj, 1, Value::Object(Some(name_s)));
        // field 2 unused in these tests
        ctx.set_field(obj, 3, Value::Int(modifiers));
        ctx.set_field(obj, 4, Value::Int(slot_index));
        let desc_s = ctx.create_string(descriptor);
        ctx.set_field(obj, 5, Value::Object(Some(desc_s)));
        ctx.set_field(obj, 6, Value::Int(1)); // accessible = true, bypass JPMS
        // Extra-slot metadata (matches create_field_object's layout).
        let base = FIELD_NUM_FIELDS;
        ctx.set_field(
            obj,
            base + FIELD_EXTRA_OFFSET_DESC,
            Value::Object(Some(desc_s)),
        );
        ctx.set_field(
            obj,
            base + FIELD_EXTRA_OFFSET_RJ_SLOT,
            Value::Int(slot_index),
        );
        ctx.set_field(
            obj,
            base + FIELD_EXTRA_OFFSET_ACCESSIBLE,
            Value::Int(1),
        );
        obj
    }

    #[test]
    fn field_set_byte_masks_to_signed_8_bits() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "b", "B", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        // Write 0x1_FF (511) — must narrow to signed byte value = -1.
        let r = native_field_set_byte(
            &mut ctx,
            &[
                Value::Object(Some(field)),
                Value::Object(Some(target)),
                Value::Int(0x1FF),
            ],
        );
        assert!(r.is_ok(), "setByte should succeed");
        assert_eq!(ctx.get_field(target, 0), Value::Int(-1));
    }

    #[test]
    fn field_set_short_masks_to_signed_16_bits() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "s", "S", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        // 0x1_8001 narrows to signed short -32767.
        let r = native_field_set_short(
            &mut ctx,
            &[
                Value::Object(Some(field)),
                Value::Object(Some(target)),
                Value::Int(0x1_8001_u32 as i32),
            ],
        );
        assert!(r.is_ok());
        assert_eq!(ctx.get_field(target, 0), Value::Int(-32767));
    }

    #[test]
    fn field_set_char_masks_to_unsigned_16_bits() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "c", "C", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        // 0x1_FFFF narrows to unsigned 16-bit 0xFFFF = 65535.
        let r = native_field_set_char(
            &mut ctx,
            &[
                Value::Object(Some(field)),
                Value::Object(Some(target)),
                Value::Int(0x1_FFFF),
            ],
        );
        assert!(r.is_ok());
        assert_eq!(ctx.get_field(target, 0), Value::Int(0xFFFF));
    }

    #[test]
    fn field_get_byte_sign_extends() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "b", "B", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        // Stored as int 0xFF (255) — getByte must reinterpret as -1.
        ctx.set_field(target, 0, Value::Int(0xFF));
        let r = native_field_get_byte(
            &mut ctx,
            &[Value::Object(Some(field)), Value::Object(Some(target))],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn field_get_char_zero_extends() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "c", "C", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(target, 0, Value::Int(-1)); // 0xFFFF_FFFF
        let r = native_field_get_char(
            &mut ctx,
            &[Value::Object(Some(field)), Value::Object(Some(target))],
        );
        // Char masks to 0xFFFF = 65535.
        assert_eq!(r.unwrap(), Some(Value::Int(0xFFFF)));
    }

    #[test]
    fn field_get_int_rejects_long_field() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "l", "J", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(target, 0, Value::Long(42));
        let r = native_field_get_int(
            &mut ctx,
            &[Value::Object(Some(field)), Value::Object(Some(target))],
        );
        assert!(r.is_err(), "getInt on long field must throw IAE");
    }

    #[test]
    fn field_get_long_widens_int() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "i", "I", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(target, 0, Value::Int(42));
        let r = native_field_get_long(
            &mut ctx,
            &[Value::Object(Some(field)), Value::Object(Some(target))],
        );
        assert_eq!(r.unwrap(), Some(Value::Long(42)));
    }

    #[test]
    fn field_get_boolean_rejects_int_field() {
        let mut ctx = mock_ctx();
        let field = make_field_mirror(&mut ctx, "x", "I", ACC_PUBLIC, 0);
        let target = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(target, 0, Value::Int(1));
        let r = native_field_get_boolean(
            &mut ctx,
            &[Value::Object(Some(field)), Value::Object(Some(target))],
        );
        assert!(r.is_err(), "getBoolean on non-boolean field must throw IAE");
    }

    // -----------------------------------------------------------------------
    // C5 — Field.getDeclaringClass must return the *declared* class mirror
    //
    // Regression test for the FieldLookup reproducer:
    //   java.lang.reflect.Field f = String.class.getDeclaredField("serialVersionUID");
    //   // f.getDeclaringClass().getName() must equal "java.lang.String", not
    //   // "java.lang.Object" (the pre-fix bug).
    // -----------------------------------------------------------------------
    #[test]
    fn c5_field_get_declaring_class_returns_declared_not_object() {
        use rustjvm_native_api::FieldMetadata;

        let mut ctx = mock_ctx();
        // Allocate a class id by initializing the declaring class name.
        let declaring_cid = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("mock ensure_class_initialized must succeed");

        // Build a FieldMetadata for a static final long field. Access
        // flags = 0x1A → ACC_PRIVATE | ACC_STATIC | ACC_FINAL (0x2|0x8|0x10).
        let meta = FieldMetadata {
            name: "serialVersionUID".to_string(),
            descriptor: "J".to_string(),
            access_flags: 0x1A,
            slot_index: 3,
            declaring_class_id: declaring_cid,
            is_static: true,
        };

        let field_obj = create_field_object(&mut ctx, &meta);

        // getDeclaringClass → should return the String class mirror whose
        // name is "java/lang/String", NOT "java/lang/Object".
        let r = native_field_get_declaring_class(
            &mut ctx,
            &[Value::Object(Some(field_obj))],
        )
        .unwrap();
        let mirror = match r {
            Some(Value::Object(Some(m))) => m,
            other => panic!("expected declaring class mirror, got {other:?}"),
        };
        let name = mirror_class_name(&ctx, mirror).unwrap_or_default();
        assert_eq!(name, "java/lang/String",
            "C5: Field.getDeclaringClass must resolve to the declared class");

        // getModifiers → must read our access flags (verifies we don't
        // land on the wrong JDK Field slot, which would previously return
        // slot_index or 0).
        let mods = native_field_get_modifiers(
            &mut ctx,
            &[Value::Object(Some(field_obj))],
        )
        .unwrap();
        assert_eq!(mods, Some(Value::Int(0x1A)),
            "C5: Field.getModifiers must return the original access flags");

        // getName → "serialVersionUID".
        let n = native_field_get_name(
            &mut ctx,
            &[Value::Object(Some(field_obj))],
        )
        .unwrap();
        let name_obj = match n {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected name String, got {other:?}"),
        };
        assert_eq!(
            ctx.read_string(name_obj).as_deref(),
            Some("serialVersionUID"),
        );

        // read_field_meta must discriminate static vs instance correctly.
        let (is_static, cid, slot, desc) = read_field_meta(&ctx, field_obj);
        assert!(is_static, "C5: static flag (0x8) must be honoured");
        assert_eq!(cid, declaring_cid);
        assert_eq!(slot, 3);
        assert_eq!(desc, "J");
    }

    // -----------------------------------------------------------------------
    // C6 — Method.getDeclaringClass must return the declared class mirror.
    //
    // Regression test for the MRTest reproducer:
    //   Method m = String.class.getDeclaredMethod("length");
    //   // m.getDeclaringClass().getName() must equal "java.lang.String".
    // Previously the Method object was allocated with only 8 synthetic
    // slots; when real-JDK bytecode did `Getfield clazz` at the inherited
    // layout offset (past slot 0) it read stale `Int(1)` residue and
    // produced the `expected object reference, got int(1)` crash.
    // -----------------------------------------------------------------------
    #[test]
    fn c6_method_get_declaring_class_returns_declared_not_object() {
        use rustjvm_native_api::MethodMetadata;

        let mut ctx = mock_ctx();
        let declaring_cid = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("mock ensure_class_initialized must succeed");

        // MethodMetadata for `public int java.lang.String.length()`.
        // Access flags = ACC_PUBLIC (0x1).
        let meta = MethodMetadata {
            name: "length".to_string(),
            descriptor: "()I".to_string(),
            access_flags: 0x1,
            declaring_class_id: declaring_cid,
            exceptions: Vec::new(),
        };

        let method_obj = create_method_object(&mut ctx, &meta);

        // getDeclaringClass → returns the String class mirror (name
        // "java/lang/String"), NOT java/lang/Object.
        let r = native_method_get_declaring_class(
            &mut ctx,
            &[Value::Object(Some(method_obj))],
        )
        .unwrap();
        let mirror = match r {
            Some(Value::Object(Some(m))) => m,
            other => panic!("expected declaring class mirror, got {other:?}"),
        };
        let name = mirror_class_name(&ctx, mirror).unwrap_or_default();
        assert_eq!(name, "java/lang/String",
            "C6: Method.getDeclaringClass must resolve to the declared class");

        // getModifiers → ACC_PUBLIC.
        let mods = native_method_get_modifiers(
            &mut ctx,
            &[Value::Object(Some(method_obj))],
        )
        .unwrap();
        assert_eq!(mods, Some(Value::Int(0x1)),
            "C6: Method.getModifiers must return the original access flags");

        // getName → "length".
        let n = native_method_get_name(
            &mut ctx,
            &[Value::Object(Some(method_obj))],
        )
        .unwrap();
        let name_obj = match n {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected name String, got {other:?}"),
        };
        assert_eq!(
            ctx.read_string(name_obj).as_deref(),
            Some("length"),
        );

        // RustJVM extra-slot descriptor must round-trip.
        let desc = read_method_descriptor(&ctx, method_obj).unwrap_or_default();
        assert_eq!(desc, "()I",
            "C6: Method raw descriptor must survive in the extra slot");

        // Parameter count is 0.
        let pc = native_method_get_parameter_count(
            &mut ctx,
            &[Value::Object(Some(method_obj))],
        )
        .unwrap();
        assert_eq!(pc, Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // T19.N1: Class security natives — getProtectionDomain0 / getSigners /
    // setSigners.  These exercise the CodeSource-driven code paths introduced
    // in Session 86 plus the synthetic ProtectionDomain allocation added
    // alongside the natives.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_n1_class_get_protection_domain0_bootstrap_returns_null() {
        // Bootstrap / jimage classes have no CodeSource URL; the mock's
        // default `class_code_base` returns None → getProtectionDomain0
        // must surface null (matches HotSpot for boot classes).
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "java/lang/Object");
        let r = native_class_get_protection_domain0(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Object(None)),
            "bootstrap class (no CodeSource URL) must return null PD");
    }

    #[test]
    fn t19_n1_class_get_protection_domain0_with_code_source_returns_pd() {
        // Simulate a class loaded from `file:/opt/app.jar` with two signer
        // cert blocks. getProtectionDomain0 should return a non-null PD
        // whose first slot (codesource) is non-null and whose codesource's
        // first slot (location) is a String equal to the URL.
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Signed").unwrap();
        // Seed overrides for this class.
        unsafe {
            (*ctx.code_base_override.get())
                .insert(cid.as_u32(), "file:/opt/app.jar".to_string());
            (*ctx.code_source_certs_override.get())
                .insert(cid.as_u32(), vec![vec![1, 2, 3], vec![4, 5]]);
        }
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Signed");
        let r = native_class_get_protection_domain0(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .unwrap();
        let pd = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected ProtectionDomain, got {other:?}"),
        };
        // PD.codesource must be non-null
        let cs = match ctx.get_field(pd, 0) {
            Value::Object(Some(c)) => c,
            other => panic!("expected CodeSource at pd[0], got {other:?}"),
        };
        // CS.location (slot 0) must be a String equal to the URL
        let loc = match ctx.get_field(cs, 0) {
            Value::Object(Some(s)) => s,
            other => panic!("expected location String at cs[0], got {other:?}"),
        };
        assert_eq!(ctx.read_string(loc).as_deref(), Some("file:/opt/app.jar"),
            "CodeSource.location must equal the class code base URL");
        // CS.certs (slot 1) must be a 2-element Object[]
        let certs_arr = match ctx.get_field(cs, 1) {
            Value::Object(Some(a)) => a,
            other => panic!("expected certs array at cs[1], got {other:?}"),
        };
        assert_eq!(ctx.array_length(certs_arr), 2,
            "CodeSource.certs must have 2 elements matching the override");
    }

    #[test]
    fn t19_n1_class_get_signers_unsigned_returns_null() {
        // No cert override → unsigned class → getSigners returns null.
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Unsigned").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Unsigned");
        let r = native_class_get_signers(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Object(None)),
            "unsigned class must return null signers array");
    }

    #[test]
    fn t19_n1_class_get_signers_with_certs_returns_byte_arrays() {
        // Seed a class with two signer blocks — getSigners must return an
        // Object[2] whose elements are byte[] copies of the raw cert bytes.
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/Signed").unwrap();
        let cert_a = vec![0x30, 0x82, 0x01, 0xA3];
        let cert_b = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE];
        unsafe {
            (*ctx.code_source_certs_override.get())
                .insert(cid.as_u32(), vec![cert_a.clone(), cert_b.clone()]);
        }
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/Signed");
        let r = native_class_get_signers(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .unwrap();
        let arr = match r {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected signers Object[], got {other:?}"),
        };
        assert_eq!(ctx.array_length(arr), 2,
            "signers array length must match cert count");
        // Element 0: byte[] of cert_a
        let e0 = match ctx.get_array_element(arr, 0) {
            Value::Object(Some(b)) => b,
            other => panic!("expected byte[] at signers[0], got {other:?}"),
        };
        assert_eq!(ctx.array_length(e0), cert_a.len());
        for (i, &b) in cert_a.iter().enumerate() {
            assert_eq!(ctx.get_array_element(e0, i), Value::Int(b as i8 as i32),
                "signers[0][{i}] must match cert_a[{i}]");
        }
        // Element 1: byte[] of cert_b
        let e1 = match ctx.get_array_element(arr, 1) {
            Value::Object(Some(b)) => b,
            other => panic!("expected byte[] at signers[1], got {other:?}"),
        };
        assert_eq!(ctx.array_length(e1), cert_b.len());
        for (i, &b) in cert_b.iter().enumerate() {
            assert_eq!(ctx.get_array_element(e1, i), Value::Int(b as i8 as i32),
                "signers[1][{i}] must match cert_b[{i}]");
        }
    }

    #[test]
    fn t19_n1_class_set_signers_stores_or_documents_noop() {
        // setSigners is a documented no-op. We just verify it does not
        // panic / throw on a valid Class mirror + non-null signers array,
        // and does not perturb the getSigners result afterward.
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("com/example/SetSignersTarget").unwrap();
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "com/example/SetSignersTarget");
        // Allocate a fake signers array to pass in.
        let signers = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 1);
        let dummy_cert = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 2);
        ctx.set_array_element(dummy_cert, 0, Value::Int(0x42));
        ctx.set_array_element(dummy_cert, 1, Value::Int(0x43));
        ctx.set_array_element(signers, 0, Value::Object(Some(dummy_cert)));
        let r = native_class_set_signers(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(signers))],
        )
        .unwrap();
        assert_eq!(r, None, "setSigners returns void (None)");
        // After the documented-noop setSigners, getSigners still reflects
        // the (absent) override → null.
        let g = native_class_get_signers(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .unwrap();
        assert_eq!(g, Some(Value::Object(None)),
            "setSigners is documented no-op; getSigners remains null");
    }

    // -----------------------------------------------------------------------
    // T19_H10: resource lookup hardening + Class.getPackage()
    //
    // These tests cover the Keycloak `org.keycloak.common.Version.<clinit>`
    // codepath:
    //   `Class.getResourceAsStream("/keycloak-version.properties")` →
    //   bytes → `Properties.load(InputStream)`. The pre-T19.H10 stub
    //   returned a stream that LineReader could read, but the resource
    //   validation gates and `Class.getPackage()` were missing, causing
    //   downstream NPEs in libraries that probe `getImplementationVersion()`.
    // -----------------------------------------------------------------------

    fn make_class_mirror_with_package(
        ctx: &mut crate::test_utils::MockNativeContext,
        class_id: u32,
        name: &str,
    ) -> ObjectRef {
        let name_obj = ctx.create_string(name);
        let mirror = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(mirror, 0, Value::Int(class_id as i32));
        ctx.set_field(mirror, 1, Value::Object(Some(name_obj)));
        mirror
    }

    #[test]
    fn t19_h10_validate_resource_name_accepts_simple() {
        assert_eq!(
            t19_h10_validate_resource_name("keycloak-version.properties"),
            Some("keycloak-version.properties")
        );
        assert_eq!(
            t19_h10_validate_resource_name("META-INF/services/java.security.Provider"),
            Some("META-INF/services/java.security.Provider")
        );
    }

    #[test]
    fn t19_h10_validate_resource_name_rejects_empty_and_oversize() {
        assert!(t19_h10_validate_resource_name("").is_none(),
            "empty name must be rejected");
        let big = "a".repeat(257);
        assert!(t19_h10_validate_resource_name(&big).is_none(),
            "name >256 bytes must be rejected");
        let edge = "a".repeat(256);
        assert!(t19_h10_validate_resource_name(&edge).is_some(),
            "name == 256 bytes must be accepted (boundary)");
    }

    #[test]
    fn t19_h10_validate_resource_name_rejects_traversal_and_backslash() {
        assert!(t19_h10_validate_resource_name("../etc/passwd").is_none(),
            "leading `..` segment must be rejected");
        assert!(t19_h10_validate_resource_name("foo/../bar").is_none(),
            "embedded `..` segment must be rejected");
        assert!(t19_h10_validate_resource_name("foo\\bar").is_none(),
            "backslash must be rejected (Windows-path-injection)");
        // `..` substring inside a filename is fine — only the segment is.
        assert!(t19_h10_validate_resource_name("foo..bar.txt").is_some(),
            "`..` substring inside filename must be accepted");
    }

    #[test]
    fn t19_h10_validate_resource_name_rejects_control_bytes() {
        assert!(t19_h10_validate_resource_name("foo\0bar").is_none(),
            "NUL byte must be rejected");
        assert!(t19_h10_validate_resource_name("foo\nbar").is_none(),
            "newline must be rejected");
        assert!(t19_h10_validate_resource_name("foo\x7Fbar").is_none(),
            "DEL byte must be rejected");
        assert!(t19_h10_validate_resource_name("foo\x1Bbar").is_none(),
            "ESC byte must be rejected");
    }

    #[test]
    fn t19_h10_get_resource_as_stream_returns_bytes_for_classpath_resource() {
        let mut ctx = mock_ctx();
        // Mimic the Keycloak Version.<clinit> path: a 28-byte properties
        // blob keyed under `keycloak-version.properties`.
        let blob = b"version=26.2.4\nbuild-time=ok\n".to_vec();
        ctx.set_resource("keycloak-version.properties", blob.clone());
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let name = ctx.create_string("/keycloak-version.properties");
        let r = native_class_get_resource_as_stream(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ).unwrap();
        let stream = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null InputStream, got {other:?}"),
        };
        // Verify the stream's `count` slot holds the right byte count and
        // `buf` slot holds the byte array.
        let count = ctx.get_field(stream, 3).as_int().unwrap_or(-1);
        assert_eq!(count, blob.len() as i32, "BAIS.count must equal blob.len()");
        let buf = match ctx.get_field(stream, 0) {
            Value::Object(Some(arr)) => arr,
            other => panic!("expected non-null buf, got {other:?}"),
        };
        // First byte of the blob is `v` (0x76).
        let first = ctx.get_array_element(buf, 0).as_int().unwrap_or(0);
        assert_eq!(first as u8, b'v', "first byte must be 'v'");
    }

    #[test]
    fn t19_h10_get_resource_as_stream_returns_null_for_missing() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let name = ctx.create_string("/no-such-resource.txt");
        let r = native_class_get_resource_as_stream(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ).unwrap();
        assert_eq!(r, Some(Value::Object(None)),
            "missing resource must return null InputStream");
    }

    #[test]
    fn t19_h10_get_resource_as_stream_rejects_traversal_name() {
        let mut ctx = mock_ctx();
        // Even if a malicious classpath plants a resource at the traversal
        // target, the validation gate must short-circuit BEFORE find_resource
        // is consulted.
        ctx.set_resource("../../../etc/passwd", b"oops".to_vec());
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let name = ctx.create_string("/../../../etc/passwd");
        let r = native_class_get_resource_as_stream(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ).unwrap();
        assert_eq!(r, Some(Value::Object(None)),
            "traversal name must be rejected, returning null");
    }

    #[test]
    fn t19_h10_get_resource_as_stream_relative_uses_package_prefix() {
        let mut ctx = mock_ctx();
        // For `Foo.class.getResourceAsStream("bar.txt")` the resolved path
        // is the package path joined with `bar.txt` — i.e. the same as
        // `Foo.class.getResourceAsStream("/org/keycloak/common/bar.txt")`.
        ctx.set_resource("org/keycloak/common/bar.txt", b"hello".to_vec());
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let name = ctx.create_string("bar.txt");
        let r = native_class_get_resource_as_stream(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ).unwrap();
        let stream = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null InputStream, got {other:?}"),
        };
        let count = ctx.get_field(stream, 3).as_int().unwrap_or(-1);
        assert_eq!(count, 5, "BAIS.count must equal len('hello')");
    }

    #[test]
    fn t19_h10_get_resource_returns_url_for_present_resource() {
        let mut ctx = mock_ctx();
        ctx.set_resource("keycloak-version.properties", b"version=26.2.4".to_vec());
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let name = ctx.create_string("/keycloak-version.properties");
        let r = native_class_get_resource(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ).unwrap();
        let url = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null URL, got {other:?}"),
        };
        // The URL synthetic stores file at field by-name `file` — but the
        // mock context's set_field_by_name routes back to slot lookups
        // that the mock doesn't model.  Instead verify the URL is non-null
        // (pre-T19.H10 returned null even when the resource was present).
        let _ = url; // proves we got a non-null mirror
    }

    #[test]
    fn t19_h10_get_resource_returns_null_for_missing() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let name = ctx.create_string("/no-such.properties");
        let r = native_class_get_resource(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ).unwrap();
        assert_eq!(r, Some(Value::Object(None)),
            "absent resource must return null URL");
    }

    #[test]
    fn t19_h10_get_package_returns_non_null_package_object() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("org/keycloak/common/Version").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "org/keycloak/common/Version");
        let r = native_class_get_package(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        ).unwrap();
        let pkg = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null Package, got {other:?}"),
        };
        // Package's `name` slot (slot 0) must hold the dotted package name.
        let name_obj = match ctx.get_field(pkg, 0) {
            Value::Object(Some(s)) => s,
            other => panic!("expected non-null name string, got {other:?}"),
        };
        assert_eq!(ctx.read_string(name_obj).unwrap(), "org.keycloak.common");
    }

    #[test]
    fn t19_h10_get_package_default_package_returns_empty_name() {
        // Class without a package (e.g. `Foo` at the default package) gets
        // a Package with empty name.
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("Foo").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "Foo");
        let r = native_class_get_package(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        ).unwrap();
        let pkg = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null Package, got {other:?}"),
        };
        let name_obj = match ctx.get_field(pkg, 0) {
            Value::Object(Some(s)) => s,
            other => panic!("expected non-null name string, got {other:?}"),
        };
        assert_eq!(ctx.read_string(name_obj).unwrap(), "",
            "default package's name is the empty string");
    }

    #[test]
    fn t19_h10_get_package_returns_null_for_primitive_array() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("[I").unwrap();
        let mirror = make_class_mirror_with_package(&mut ctx, cid.as_u32(), "[I");
        let r = native_class_get_package(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        ).unwrap();
        assert_eq!(r, Some(Value::Object(None)),
            "primitive-component array has no Package");
    }

    #[test]
    fn t19_h10_alloc_byte_array_input_stream_layout() {
        let mut ctx = mock_ctx();
        let bytes = b"hello".to_vec();
        let stream = t19_h10_alloc_byte_array_input_stream(&mut ctx, &bytes);
        let count = ctx.get_field(stream, 3).as_int().unwrap_or(-1);
        assert_eq!(count, 5);
        let pos = ctx.get_field(stream, 1).as_int().unwrap_or(-1);
        assert_eq!(pos, 0);
        let buf = match ctx.get_field(stream, 0) {
            Value::Object(Some(arr)) => arr,
            other => panic!("expected non-null buf, got {other:?}"),
        };
        // Round-trip every byte.
        for (i, &b) in bytes.iter().enumerate() {
            let v = ctx.get_array_element(buf, i).as_int().unwrap_or(0);
            assert_eq!(v as u8, b, "byte {i} mismatch");
        }
    }

    // -----------------------------------------------------------------------
    // F2 — `Class.getDeclaredMethod` reflection visibility for
    // `ClassLoader.defineClass` overloads (WP2.3 follow-up).
    //
    // CGLIB's `ReflectUtils.<clinit>` does:
    //   ClassLoader.class.getDeclaredMethod(
    //       "defineClass",
    //       String.class, byte[].class, int.class, int.class,
    //       ProtectionDomain.class);
    //
    // Before this fix, `java/lang/ClassLoader` was loaded as a synthetic
    // stub (no `.class` file shipped in synthetic-jdk mode), so its
    // `class.methods` was empty → `declared_methods` returned an empty
    // list → `getDeclaredMethod` threw `NoSuchMethodException`.  The fix
    // augments `declared_methods` for known synthetic-stub JDK classes
    // with their canonical method declarations, surfacing the JDK
    // contract through reflection without requiring real bytecode.
    // -----------------------------------------------------------------------

    /// Helper: build a `Class[]` mirror array describing the param types
    /// that CGLIB's lookup uses for the 5-arg ClassLoader.defineClass.
    fn make_param_array_5arg_define_class(
        ctx: &mut crate::test_utils::MockNativeContext,
    ) -> ObjectRef {
        // Order matches the descriptor:
        //   (Ljava/lang/String;[BIILjava/security/ProtectionDomain;)
        let descs = [
            "Ljava/lang/String;",
            "[B",
            "I",
            "I",
            "Ljava/security/ProtectionDomain;",
        ];
        let arr = ctx.new_ref_array(ClassId::new(0), descs.len());
        for (i, d) in descs.iter().enumerate() {
            let mirror = descriptor_to_class_mirror(ctx, d);
            ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
        }
        arr
    }

    #[test]
    fn f2_synthetic_decls_table_includes_classloader_define_class_5arg() {
        // Table-only check: the augmentation table must declare the
        // 5-arg `defineClass(String, byte[], int, int, ProtectionDomain)`.
        let table = synthetic_jdk_method_decls("java/lang/ClassLoader");
        let target_desc =
            "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;";
        let hit = table.iter().find(|(name, desc, _)| {
            *name == "defineClass" && *desc == target_desc
        });
        assert!(
            hit.is_some(),
            "F2: synthetic decls table must include the 5-arg \
             ClassLoader.defineClass(String,byte[],int,int,ProtectionDomain)",
        );
    }

    #[test]
    fn f2_synthetic_decls_table_returns_empty_for_unknown_class() {
        let table = synthetic_jdk_method_decls("com/example/UserClass");
        assert!(table.is_empty(),
            "F2: synthetic augmentation must NOT apply to user classes");
    }

    #[test]
    fn f2_get_declared_method_finds_classloader_define_class_5arg() {
        let mut ctx = mock_ctx();
        let cl_cid = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("mock ensure_class_initialized must succeed");
        let mirror = make_class_mirror(&mut ctx, cl_cid.as_u32(), "java/lang/ClassLoader");
        let name = ctx.create_string("defineClass");
        let params = make_param_array_5arg_define_class(&mut ctx);

        let r = native_class_get_declared_method(
            &mut ctx,
            &[
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(Some(params)),
            ],
        )
        .expect(
            "F2: getDeclaredMethod must NOT throw NSME for ClassLoader.defineClass(5-arg)",
        );

        let method_obj = match r {
            Some(Value::Object(Some(m))) => m,
            other => panic!("F2: expected non-null Method, got {other:?}"),
        };

        // Spot-check the descriptor extra-slot round-trips.
        let desc = read_method_descriptor(&ctx, method_obj).unwrap_or_default();
        assert_eq!(
            desc,
            "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;",
            "F2: returned Method must carry the 5-arg descriptor",
        );
    }

    #[test]
    fn f2_get_declared_method_finds_classloader_define_class_4arg_no_pd() {
        // The 4-arg `defineClass(String, byte[], int, int)` is `protected`
        // in OpenJDK; reflection still surfaces it on getDeclaredMethod
        // (visibility checks come later, on Method.invoke).
        let mut ctx = mock_ctx();
        let cl_cid = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("mock ensure_class_initialized must succeed");
        let mirror = make_class_mirror(&mut ctx, cl_cid.as_u32(), "java/lang/ClassLoader");
        let name = ctx.create_string("defineClass");

        // 4 params: String, byte[], int, int.
        let descs = ["Ljava/lang/String;", "[B", "I", "I"];
        let arr = ctx.new_ref_array(ClassId::new(0), descs.len());
        for (i, d) in descs.iter().enumerate() {
            let m = descriptor_to_class_mirror(&mut ctx, d);
            ctx.set_array_element(arr, i, Value::Object(Some(m)));
        }

        let r = native_class_get_declared_method(
            &mut ctx,
            &[
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
            ],
        )
        .expect(
            "F2: getDeclaredMethod must find the 4-arg defineClass on ClassLoader",
        );

        let method_obj = match r {
            Some(Value::Object(Some(m))) => m,
            other => panic!("F2: expected non-null Method for 4-arg, got {other:?}"),
        };
        let desc = read_method_descriptor(&ctx, method_obj).unwrap_or_default();
        assert_eq!(
            desc,
            "(Ljava/lang/String;[BII)Ljava/lang/Class;",
            "F2: returned Method must be the 4-arg overload",
        );
    }

    #[test]
    fn f2_get_declared_methods_returns_classloader_define_class_overloads() {
        // getDeclaredMethods (no name filter) must include all four
        // defineClass overloads from the synthetic decls table.
        let mut ctx = mock_ctx();
        let cl_cid = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("mock ensure_class_initialized must succeed");
        let mirror = make_class_mirror(&mut ctx, cl_cid.as_u32(), "java/lang/ClassLoader");

        let r = native_class_get_declared_methods(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .expect("F2: getDeclaredMethods must succeed for ClassLoader");

        let arr = match r {
            Some(Value::Object(Some(a))) => a,
            other => panic!("F2: expected non-null array, got {other:?}"),
        };
        let len = ctx.array_length(arr);
        assert!(len > 0, "F2: getDeclaredMethods must return >0 methods");

        // Collect all descriptors that match name "defineClass".
        let mut define_class_descs = Vec::new();
        for i in 0..len {
            let mobj = match ctx.get_array_element(arr, i) {
                Value::Object(Some(o)) => o,
                _ => continue,
            };
            let nv = ctx.get_field_by_name(mobj, "name");
            let name_str = match nv {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if name_str != "defineClass" {
                continue;
            }
            let desc = read_method_descriptor(&ctx, mobj).unwrap_or_default();
            define_class_descs.push(desc);
        }

        assert!(
            define_class_descs.iter().any(|d| d == "([BII)Ljava/lang/Class;"),
            "F2: legacy 3-arg defineClass([B,I,I) overload missing — got {:?}",
            define_class_descs,
        );
        assert!(
            define_class_descs
                .iter()
                .any(|d| d == "(Ljava/lang/String;[BII)Ljava/lang/Class;"),
            "F2: 4-arg defineClass(String,[B,I,I) overload missing — got {:?}",
            define_class_descs,
        );
        assert!(
            define_class_descs.iter().any(|d| d
                == "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;"),
            "F2: 5-arg defineClass(...,ProtectionDomain) overload missing — got {:?}",
            define_class_descs,
        );
        assert!(
            define_class_descs.iter().any(|d| d
                == "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;"),
            "F2: 3-arg ByteBuffer defineClass overload missing — got {:?}",
            define_class_descs,
        );
    }

    #[test]
    fn f2_synthetic_decls_do_not_clobber_real_class_methods() {
        // Real bytecode declarations take precedence — the augmentation
        // must skip a method already present in `declared_methods`.
        let mut ctx = mock_ctx();
        let cl_cid = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("mock ensure_class_initialized must succeed");

        // Inject a "real" methods record for ClassLoader containing only the
        // 5-arg defineClass — but with different access flags (e.g. ACC_PUBLIC
        // = 0x1 instead of synthetic's protected/final = 0x14). The merged
        // result must keep the real flags, not duplicate.
        let real_meta = MethodMetadata {
            name: "defineClass".to_string(),
            descriptor:
                "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;"
                    .to_string(),
            access_flags: 0x1, // ACC_PUBLIC, distinct from synthetic 0x14
            declaring_class_id: cl_cid,
            exceptions: Vec::new(),
        };
        ctx.set_declared_methods(cl_cid, vec![real_meta]);

        let merged = declared_methods_with_synthetic(&ctx, cl_cid);
        let five_arg: Vec<&MethodMetadata> = merged
            .iter()
            .filter(|m| {
                m.name == "defineClass"
                    && m.descriptor
                        == "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;"
            })
            .collect();
        assert_eq!(
            five_arg.len(),
            1,
            "F2: real-class declaration must not be duplicated by synthetic augmentation",
        );
        assert_eq!(
            five_arg[0].access_flags, 0x1,
            "F2: real-class flags (0x1 ACC_PUBLIC) must win over synthetic flags (0x14)",
        );
    }

    // -----------------------------------------------------------------------
    // G2 — `Class.getDeclaredMethods0` returning null array (ByteBuddy +
    // Mockito blocker after WP2.3+F3 landed).
    //
    // After F3 cleared the verifier false positive, ByteBuddy advanced
    // past `ClassFileDumper.<clinit>` and reached `JavaDispatcher.<clinit>`
    // which iterates `Method.class.getDeclaredMethods()`. The iteration
    // does `arraylength` on either the returned array or one of its
    // Method elements' fields (e.g. `parameterTypes`, `exceptionTypes`,
    // `annotations`). G2 ensures:
    //   * `getDeclaredMethods0` for `java/lang/reflect/Method` returns a
    //     non-null, non-empty array (synthetic decls for canonical
    //     `getName`, `invoke`, …).
    //   * Every Method object created via `create_method_object` has all
    //     its array-typed fields initialised to non-null empty arrays
    //     (matching the real-JDK constructor invariant).
    //   * The same applies to `Field`, `Constructor`, and `Executable` —
    //     so frameworks like Mockito that reflect on these classes
    //     don't NPE either.
    // -----------------------------------------------------------------------

    #[test]
    fn g2_synthetic_decls_table_includes_method_class() {
        let table = synthetic_jdk_method_decls("java/lang/reflect/Method");
        assert!(
            !table.is_empty(),
            "G2: synthetic decls table must include java/lang/reflect/Method",
        );
        let has_invoke = table.iter().any(|(name, desc, _)| {
            *name == "invoke"
                && *desc
                    == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
        });
        assert!(has_invoke, "G2: Method.invoke must be in the synthetic decls table");
        let has_get_name = table.iter().any(|(name, desc, _)| {
            *name == "getName" && *desc == "()Ljava/lang/String;"
        });
        assert!(has_get_name, "G2: Method.getName must be in the synthetic decls table");
    }

    #[test]
    fn g2_synthetic_decls_table_includes_field_class() {
        let table = synthetic_jdk_method_decls("java/lang/reflect/Field");
        assert!(
            !table.is_empty(),
            "G2: synthetic decls table must include java/lang/reflect/Field",
        );
        let has_get = table
            .iter()
            .any(|(name, desc, _)| *name == "get" && *desc == "(Ljava/lang/Object;)Ljava/lang/Object;");
        assert!(has_get, "G2: Field.get must be in the synthetic decls table");
    }

    #[test]
    fn g2_synthetic_decls_table_includes_constructor_class() {
        let table = synthetic_jdk_method_decls("java/lang/reflect/Constructor");
        assert!(
            !table.is_empty(),
            "G2: synthetic decls table must include java/lang/reflect/Constructor",
        );
        let has_new_instance = table.iter().any(|(name, desc, _)| {
            *name == "newInstance"
                && *desc == "([Ljava/lang/Object;)Ljava/lang/Object;"
        });
        assert!(
            has_new_instance,
            "G2: Constructor.newInstance must be in the synthetic decls table",
        );
    }

    #[test]
    fn g2_synthetic_decls_table_includes_executable_class() {
        let table = synthetic_jdk_method_decls("java/lang/reflect/Executable");
        assert!(
            !table.is_empty(),
            "G2: synthetic decls table must include java/lang/reflect/Executable",
        );
    }

    #[test]
    fn g2_get_declared_methods_for_method_class_returns_non_null_array() {
        // ByteBuddy's `JavaDispatcher.<clinit>` does:
        //   Method[] methods = Method.class.getDeclaredMethods();
        //   for (Method m : methods) { ... }
        //
        // The native MUST return a non-null array — even an empty one
        // is acceptable, but null is not.
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/reflect/Method")
            .expect("mock ensure_class_initialized must succeed");
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "java/lang/reflect/Method");

        let r = native_class_get_declared_methods(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .expect("G2: getDeclaredMethods must NOT throw on Method class");

        let arr = match r {
            Some(Value::Object(Some(a))) => a,
            other => panic!(
                "G2: getDeclaredMethods MUST return non-null array (was {other:?})",
            ),
        };
        let len = ctx.array_length(arr);
        assert!(
            len > 0,
            "G2: Method.class.getDeclaredMethods() must surface ≥1 method (canonical synthetic decls)",
        );
    }

    #[test]
    fn g2_get_declared_methods_for_method_class_includes_invoke() {
        // Stronger invariant: ByteBuddy looks for specific methods (e.g.
        // `invoke`) by name. The synthetic table guarantees `invoke` is
        // surfaced even if the real-class method list happens to be empty.
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/reflect/Method")
            .expect("mock ensure_class_initialized must succeed");
        let mirror = make_class_mirror(&mut ctx, cid.as_u32(), "java/lang/reflect/Method");

        let r = native_class_get_declared_methods(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .expect("G2: getDeclaredMethods must succeed for Method class");

        let arr = match r {
            Some(Value::Object(Some(a))) => a,
            other => panic!("G2: expected non-null array, got {other:?}"),
        };
        let len = ctx.array_length(arr);

        let mut found_invoke = false;
        let mut found_get_name = false;
        for i in 0..len {
            let mobj = match ctx.get_array_element(arr, i) {
                Value::Object(Some(o)) => o,
                _ => continue,
            };
            let nv = ctx.get_field_by_name(mobj, "name");
            let name_str = match nv {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if name_str == "invoke" {
                found_invoke = true;
            }
            if name_str == "getName" {
                found_get_name = true;
            }
        }
        assert!(found_invoke, "G2: Method.invoke must appear in getDeclaredMethods()");
        assert!(found_get_name, "G2: Method.getName must appear in getDeclaredMethods()");
    }

    #[test]
    fn g2_get_declared_methods_never_returns_null_even_on_unknown_class() {
        // Pathological case: a Class mirror that doesn't map to any
        // synthetic class. Must STILL return a non-null array (empty),
        // never `Value::Object(None)`.
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("com/example/UnknownClass")
            .expect("mock ensure_class_initialized must succeed");
        let mirror =
            make_class_mirror(&mut ctx, cid.as_u32(), "com/example/UnknownClass");

        let r = native_class_get_declared_methods(
            &mut ctx,
            &[Value::Object(Some(mirror))],
        )
        .expect("G2: getDeclaredMethods must NOT error on unknown classes");

        match r {
            Some(Value::Object(Some(_))) => {}
            other => panic!(
                "G2: getDeclaredMethods on unknown class must return non-null array (was {other:?})",
            ),
        }
    }

    #[test]
    fn g2_create_method_object_populates_exception_types_non_null() {
        // The real JDK Method constructor initializes `exceptionTypes`
        // to a non-null array; we mirror that invariant. ByteBuddy and
        // similar frameworks sometimes call `Method.getExceptionTypes()`
        // which does `exceptionTypes.clone()` — that NPEs on null.
        let mut ctx = mock_ctx();
        let declaring_cid = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("ensure_class_initialized");
        let meta = MethodMetadata {
            name: "toString".to_string(),
            descriptor: "()Ljava/lang/String;".to_string(),
            access_flags: 0x01,
            declaring_class_id: declaring_cid,
            exceptions: Vec::new(),
        };
        let m = create_method_object(&mut ctx, &meta);

        let v = ctx.get_field_by_name(m, "exceptionTypes");
        match v {
            Value::Object(Some(arr)) => {
                assert_eq!(
                    ctx.array_length(arr),
                    0,
                    "G2: empty exceptionTypes array must have length 0",
                );
            }
            other => panic!(
                "G2: Method.exceptionTypes MUST be a non-null array (was {other:?})",
            ),
        }
    }

    #[test]
    fn g2_create_method_object_populates_parameter_types_non_null() {
        // Even for a no-arg method, parameterTypes must be an empty
        // array (not null). ByteBuddy's iterator does
        // `m.getParameterTypes().length` which arraylength's null.
        let mut ctx = mock_ctx();
        let declaring_cid = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("ensure_class_initialized");
        let meta = MethodMetadata {
            name: "hashCode".to_string(),
            descriptor: "()I".to_string(),
            access_flags: 0x01,
            declaring_class_id: declaring_cid,
            exceptions: Vec::new(),
        };
        let m = create_method_object(&mut ctx, &meta);

        let v = ctx.get_field_by_name(m, "parameterTypes");
        match v {
            Value::Object(Some(arr)) => {
                assert_eq!(
                    ctx.array_length(arr),
                    0,
                    "G2: parameterTypes for ()I must have length 0",
                );
            }
            other => panic!(
                "G2: Method.parameterTypes MUST be a non-null array (was {other:?})",
            ),
        }
    }

    #[test]
    fn g2_create_method_object_populates_annotation_byte_arrays_non_null() {
        // JDK 25 `Method.annotations`, `parameterAnnotations`, and
        // `annotationDefault` are all `byte[]`. AnnotationParser walks
        // these via `arr.length`. None must be null after
        // `create_method_object`.
        let mut ctx = mock_ctx();
        let declaring_cid = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("ensure_class_initialized");
        let meta = MethodMetadata {
            name: "toString".to_string(),
            descriptor: "()Ljava/lang/String;".to_string(),
            access_flags: 0x01,
            declaring_class_id: declaring_cid,
            exceptions: Vec::new(),
        };
        let m = create_method_object(&mut ctx, &meta);

        for field in &["annotations", "parameterAnnotations", "annotationDefault"] {
            let v = ctx.get_field_by_name(m, field);
            match v {
                Value::Object(Some(arr)) => {
                    assert_eq!(
                        ctx.array_length(arr),
                        0,
                        "G2: {field} byte-array must be empty (length 0)",
                    );
                }
                other => panic!(
                    "G2: Method.{field} MUST be non-null byte[] (was {other:?})",
                ),
            }
        }
    }

    #[test]
    fn g2_get_superclass_returns_null_for_interface() {
        // Per JLS 8.1.4 / Class.getSuperclass() spec: an interface returns
        // null. ByteBuddy's JavaDispatcher relies on this so that
        // `privateGetPublicMethods()` doesn't walk into Object's public
        // 0-arg methods (which would underflow `parameterTypes.length - 1`
        // in `JavaDispatcher$DynamicClassLoader.invoker()`).
        let mut ctx = mock_ctx();
        let object_cid = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("ensure Object");
        let iface_cid = ctx
            .ensure_class_initialized("net/bytebuddy/utility/Invoker")
            .expect("ensure mock interface class");

        // The class file would have super_class = java/lang/Object,
        // so superclass_of() returns Some(Object) for an interface too.
        ctx.set_superclass(iface_cid, object_cid);
        ctx.set_is_interface(iface_cid, true);

        let mirror = ctx.get_class_mirror(iface_cid);
        let r = native_class_get_superclass(&mut ctx, &[Value::Object(Some(mirror))])
            .expect("native_class_get_superclass must succeed for interface");
        match r {
            Some(Value::Object(None)) => {} // null — correct
            other => panic!(
                "G2: getSuperclass() on an interface MUST return null (was {other:?})",
            ),
        }
    }

    #[test]
    fn g2_get_superclass_returns_parent_for_class() {
        // Sanity: a regular class still returns its declared superclass.
        let mut ctx = mock_ctx();
        let object_cid = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("ensure Object");
        let cls_cid = ctx
            .ensure_class_initialized("com/example/Foo")
            .expect("ensure Foo");
        ctx.set_superclass(cls_cid, object_cid);
        // is_interface defaults to false — no override needed.

        let mirror = ctx.get_class_mirror(cls_cid);
        let r = native_class_get_superclass(&mut ctx, &[Value::Object(Some(mirror))])
            .expect("native_class_get_superclass on class");
        match r {
            Some(Value::Object(Some(_super_mirror))) => {} // non-null — correct
            other => panic!(
                "G2: getSuperclass() on a regular class MUST return its parent (was {other:?})",
            ),
        }
    }

    #[test]
    fn i2_classloader_get_defined_package_returns_null() {
        // I2: getDefinedPackage(name) MUST return null without touching
        // the (potentially null) `packages` field. ByteBuddy's
        // `Resolver$ForModuleSystem.accept` and the JDK's `postDefineClass`
        // both null-check the result, so returning null is a clean exit
        // rather than NPE-ing on `packages.get(name)`.
        let mut ctx = mock_ctx();
        let name = ctx.create_string("java.lang");
        let r = i2_classloader_get_defined_package(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(name))],
        )
        .expect("i2_classloader_get_defined_package must succeed");
        match r {
            Some(Value::Object(None)) => {} // null — correct
            other => panic!(
                "I2: getDefinedPackage MUST return null (was {other:?})",
            ),
        }
    }

    #[test]
    fn i2_classloader_get_named_package_synthesises_package() {
        // I2: getNamedPackage(name, module) builds a synthetic Package
        // (name slot 0 set) without touching the null `packages` field.
        // postDefineClass `pop`s the result so any non-null Package object
        // satisfies its caller; we go further and stamp the name so
        // downstream `Package.getName()` is sensible.
        let mut ctx = mock_ctx();
        let pname = ctx.create_string("net.bytebuddy");
        let r = i2_classloader_get_named_package(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(pname)),
                Value::Object(None),
            ],
        )
        .expect("i2_classloader_get_named_package must succeed");
        let pkg = match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!(
                "I2: getNamedPackage MUST return a non-null Package (was {other:?})",
            ),
        };
        // Verify slot 0 (and the by-name field) carry the package name.
        let stamped = ctx.get_field(pkg, 0);
        match stamped {
            Value::Object(Some(o)) => {
                let s = ctx.read_string(o).unwrap_or_default();
                assert_eq!(s, "net.bytebuddy");
            }
            other => panic!("I2: Package name slot must be a String (was {other:?})"),
        }
    }

    #[test]
    fn i2_classloader_check_certs_is_noop() {
        // I2: checkCerts is a no-op so `preDefineClass` doesn't NPE on
        // the null `package2certs` ConcurrentHashMap. Returning Ok(None)
        // matches the void return type and signals success.
        let mut ctx = mock_ctx();
        let r = i2_classloader_check_certs(
            &mut ctx,
            &[Value::Object(None), Value::Object(None), Value::Object(None)],
        )
        .expect("i2_classloader_check_certs must succeed");
        assert!(
            r.is_none(),
            "I2: checkCerts MUST return None (void) — got {r:?}",
        );
    }

    #[test]
    fn g2_collect_public_methods_skips_object_methods_for_interface() {
        // Test that collect_public_methods (backing Class.getMethods())
        // does NOT walk Object's public methods when the entry class is
        // an interface. Real JDK semantics — see
        // `Class.privateGetPublicMethods()` source for the
        // `isInterface() ? null : getSuperclass()` pattern.
        let mut ctx = mock_ctx();
        let object_cid = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("ensure Object");
        // Give Object some 0-arg public methods so we'd see them if
        // walked.
        let object_methods = vec![
            MethodMetadata {
                name: "toString".to_string(),
                descriptor: "()Ljava/lang/String;".to_string(),
                access_flags: 0x01,
                declaring_class_id: object_cid,
                exceptions: Vec::new(),
            },
            MethodMetadata {
                name: "hashCode".to_string(),
                descriptor: "()I".to_string(),
                access_flags: 0x01,
                declaring_class_id: object_cid,
                exceptions: Vec::new(),
            },
        ];
        ctx.set_declared_methods(object_cid, object_methods);

        // Interface declaring just one method.
        let iface_cid = ctx
            .ensure_class_initialized("com/example/IFace")
            .expect("ensure IFace");
        let iface_methods = vec![MethodMetadata {
            name: "doIt".to_string(),
            descriptor: "(I)I".to_string(),
            access_flags: 0x401, // ACC_PUBLIC|ACC_ABSTRACT
            declaring_class_id: iface_cid,
            exceptions: Vec::new(),
        }];
        ctx.set_declared_methods(iface_cid, iface_methods);
        ctx.set_superclass(iface_cid, object_cid);
        ctx.set_is_interface(iface_cid, true);

        let methods = collect_public_methods(&mut ctx, iface_cid);
        // Only `doIt` from the interface — Object methods are skipped.
        assert_eq!(
            methods.len(),
            1,
            "G2: collect_public_methods on an interface must skip Object's superclass methods (got {} methods)",
            methods.len(),
        );
    }

    #[test]
    fn g2_synthetic_decls_for_method_class_does_not_clobber_real_class_methods() {
        // Real-class methods take precedence — same invariant as F2's
        // ClassLoader test, applied to Method.
        let mut ctx = mock_ctx();
        let m_cid = ctx
            .ensure_class_initialized("java/lang/reflect/Method")
            .expect("mock ensure_class_initialized must succeed");

        // Inject "real" `invoke` declaration with distinct flags.
        let real_meta = MethodMetadata {
            name: "invoke".to_string(),
            descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
                .to_string(),
            access_flags: 0x09, // ACC_PUBLIC|ACC_STATIC, distinct from synthetic 0x81
            declaring_class_id: m_cid,
            exceptions: Vec::new(),
        };
        ctx.set_declared_methods(m_cid, vec![real_meta]);

        let merged = declared_methods_with_synthetic(&ctx, m_cid);
        let invokes: Vec<&MethodMetadata> = merged
            .iter()
            .filter(|m| {
                m.name == "invoke"
                    && m.descriptor
                        == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
            })
            .collect();
        assert_eq!(
            invokes.len(),
            1,
            "G2: real-class invoke must not be duplicated by synthetic augmentation",
        );
        assert_eq!(
            invokes[0].access_flags, 0x09,
            "G2: real-class flags must win over synthetic flags",
        );
    }

    // -----------------------------------------------------------------------
    // T19_H12_ — Class.forName(Module, String) native
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h12_class_for_name_module_loads_existing_class() {
        let mut ctx = mock_ctx();
        // Pre-load a class so ensure_class_initialized succeeds.
        let _ = ctx.ensure_class_initialized("java/lang/String").unwrap();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let name_str = ctx.create_string("java.lang.String");
        let r = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(Some(name_str))],
        );
        match r.unwrap().unwrap() {
            Value::Object(Some(_mirror)) => {} // non-null Class mirror
            other => panic!("expected non-null Class mirror, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_class_for_name_module_null_module_npe() {
        let mut ctx = mock_ctx();
        let name_str = ctx.create_string("java.lang.String");
        let err = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(name_str))],
        )
        .unwrap_err();
        let s = format!("{:?}", err);
        assert!(s.contains("Null") || s.contains("null"), "expected NPE, got {}", s);
    }

    #[test]
    fn t19_h12_class_for_name_module_null_name_npe() {
        let mut ctx = mock_ctx();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let err = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(None)],
        )
        .unwrap_err();
        let s = format!("{:?}", err);
        assert!(s.contains("Null") || s.contains("null"), "expected NPE, got {}", s);
    }

    #[test]
    fn t19_h12_class_for_name_module_miss_does_not_throw_cnfe() {
        // Per JDK 25 spec, `Class.forName(Module, String)` returns null on
        // miss and does NOT throw ClassNotFoundException — distinguishing
        // it from `Class.forName(String)`. The mock's
        // `ensure_class_initialized` auto-creates classes, so we can't
        // hit a real miss here; instead we verify that the API contract
        // shape is preserved (no Err returned for any reasonable input).
        let mut ctx = mock_ctx();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let name_str = ctx.create_string("non.existent.Bogus");
        let r = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(Some(name_str))],
        );
        // Must NOT be an Err — never CNFE for this overload.
        assert!(r.is_ok(), "Class.forName(Module, String) must not throw CNFE: {:?}", r);
    }

    #[test]
    fn t19_h12_class_for_name_module_rejects_path_traversal_name() {
        // Hardening: control bytes and path separators are not valid class names.
        let mut ctx = mock_ctx();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let evil = ctx.create_string("../../etc/passwd");
        let r = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(Some(evil))],
        );
        match r {
            Ok(Some(Value::Object(None))) => {}
            other => panic!("expected null for malicious name, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_class_for_name_module_rejects_control_byte_name() {
        let mut ctx = mock_ctx();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let evil = ctx.create_string("java.lang.\0Evil");
        let r = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(Some(evil))],
        );
        match r {
            Ok(Some(Value::Object(None))) => {}
            other => panic!("expected null for control-byte name, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_class_for_name_module_empty_name_returns_null() {
        let mut ctx = mock_ctx();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let empty = ctx.create_string("");
        let r = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(Some(empty))],
        );
        match r {
            Ok(Some(Value::Object(None))) => {}
            other => panic!("expected null for empty name, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_class_for_name_module_dotted_to_internal() {
        // Verify that dotted form ("java.lang.Object") resolves to a class
        // that's already registered under internal form ("java/lang/Object").
        let mut ctx = mock_ctx();
        let _ = ctx.ensure_class_initialized("java/lang/Object").unwrap();
        let module_obj = ctx.alloc_object(ClassId::new(0), 5);
        let dotted = ctx.create_string("java.lang.Object");
        let r = native_class_for_name_module(
            &mut ctx,
            &[Value::Object(Some(module_obj)), Value::Object(Some(dotted))],
        );
        match r.unwrap().unwrap() {
            Value::Object(Some(_)) => {}
            other => panic!("expected Class mirror for java.lang.Object, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // WP2.1-field — final-write check + volatile-aware fence helpers.
    // -----------------------------------------------------------------------
    //
    // These are pure-Rust helpers, so the tests are tiny but they pin the
    // semantics that `Field.set*` enforces:
    //   * non-static final without setAccessible → IllegalAccessException
    //   * static final ALWAYS → IllegalAccessException (even with
    //     setAccessible — the escape hatch is Unsafe / VarHandle)
    //   * non-final fields → no error, regardless of accessible
    //   * volatile fences are emitted only when ACC_VOLATILE is set on
    //     the field's modifiers (avoiding the perf hit on plain reads).

    #[test]
    fn wp21_field_check_final_non_final_passes() {
        // Plain int field, public, no final bit set — should pass.
        let modifiers = ACC_PUBLIC; // 0x0001
        assert!(check_final_for_set(modifiers, false, "x").is_ok());
        assert!(check_final_for_set(modifiers, true, "x").is_ok());
    }

    #[test]
    fn wp21_field_check_final_instance_final_no_access_throws() {
        let modifiers = ACC_PUBLIC | ACC_FINAL; // 0x0011
        let r = check_final_for_set(modifiers, false, "Y");
        assert!(
            r.is_err(),
            "instance final without setAccessible MUST throw IllegalAccessException"
        );
        let err = r.unwrap_err();
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("IllegalAccessException"),
            "expected IllegalAccessException, got: {msg}"
        );
    }

    #[test]
    fn wp21_field_check_final_instance_final_with_access_passes() {
        // Mirrors `ff.setAccessible(true); ff.setInt(o, 11);` — must succeed.
        let modifiers = ACC_PUBLIC | ACC_FINAL;
        assert!(check_final_for_set(modifiers, true, "Y").is_ok());
    }

    #[test]
    fn wp21_field_check_final_static_final_always_throws() {
        let modifiers = ACC_PUBLIC | ACC_STATIC | ACC_FINAL; // 0x0019
        // Without setAccessible.
        assert!(
            check_final_for_set(modifiers, false, "K").is_err(),
            "static final without setAccessible MUST throw"
        );
        // WITH setAccessible — must still throw.
        let r = check_final_for_set(modifiers, true, "K");
        assert!(
            r.is_err(),
            "static final WITH setAccessible MUST also throw — only Unsafe / VarHandle bypasses",
        );
        let msg = format!("{:?}", r.unwrap_err());
        assert!(
            msg.contains("static final"),
            "static-final error should call out 'static final' specifically, got: {msg}",
        );
    }

    #[test]
    fn wp21_field_volatile_fence_no_op_on_plain_field() {
        // Pure runtime smoke: should not panic. The fence is unobservable
        // by definition; we just exercise the branch that decides whether
        // to issue one.
        let plain = ACC_PUBLIC; // no ACC_VOLATILE
        volatile_load_fence(plain);
        volatile_store_fence_pre(plain);
        volatile_store_fence_post(plain);
    }

    #[test]
    fn wp21_field_volatile_fence_emitted_on_volatile_field() {
        // Same shape — exercises the path that issues the fence. We can't
        // observe the memory ordering directly from a single thread, but
        // we can pin that the call doesn't panic and the bit-test is
        // exercised end-to-end. The integration test in
        // `vm/tests/wp2_1_field_surface.rs::volatile_long_round_trips`
        // pins the end-to-end round-trip via Field.getLong/setLong.
        let volatile_field = ACC_PUBLIC | ACC_VOLATILE; // 0x0041
        volatile_load_fence(volatile_field);
        volatile_store_fence_pre(volatile_field);
        volatile_store_fence_post(volatile_field);
    }
}

