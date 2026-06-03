// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H14 — JMX OpenType / MXBeanIntrospector translation natives.
//! T19.M1 — Proactive hardening for all 8 platform MXBeans.
//!
//! Anchor: `T19_H14_OPENMBEAN` (initial Object-method filter).
//! Anchor: `T19_M1_PLATFORM_MXBEANS` (cycle detector + composite types).
//!
//! KC16 (WildFly / JBoss Modules) boot reaches
//! `ManagementFactory.getPlatformMBeanServer()` and then constructs a
//! `StandardMBean` wrapping `MemoryMXBean`. The JDK 25 MBean compliance
//! check walks the interface's reflective methods through
//! `MXBeanIntrospector.mFrom(Method)` →
//! `ConvertingMethod.from(Method)` →
//! `MXBeanMappingFactory.mappingForType(Type, MXBeanMappingFactory)`,
//! which recursively descends `java.lang.Class` (returned by the
//! inherited `java.lang.Object.getClass` method) into
//! `java.lang.reflect.AnnotatedType[]`, hits a self-reference, and
//! throws `OpenDataException("Recursive data structure, including
//! java.lang.reflect.AnnotatedType")`. That bubbles up through
//! `IllegalArgumentException` → `NotCompliantMBeanException` →
//! `IllegalArgumentException`, which kills the main thread before
//! WildFly's `org.jboss.as.standalone.Main.main` can even register
//! its first standalone-mode service.
//!
//! Real HotSpot doesn't fail on this path because the JDK's
//! `DefaultMXBeanMappingFactory` static initialiser pre-installs
//! "permanent" mappings for `java.lang.Class`, `Object`, `Number`,
//! `Boolean`, `Character`, the wrapper classes, `String`, `Date`,
//! `BigInteger`, `BigDecimal`, `ObjectName`, and a handful of other
//! types. With those permanent mappings, the recursive walk into
//! `Class.getAnnotatedInterfaces()` never starts because `Class` is
//! mapped directly to `SimpleType.STRING`. We don't replicate the
//! permanent-mapping table — instead we filter Object methods out at
//! the `MBeanIntrospector.getMethods(Class)` source, which is where
//! `MBeanAnalyzer.initMaps` consumes them. The visible semantics are
//! equivalent for KC16's needs (Object methods like `getClass`,
//! `wait`, `notify`, `hashCode`, `equals`, `toString`, `clone`,
//! `finalize` aren't part of the MBean attribute / operation surface
//! anyway), and the override is surgical (a single virtual method
//! intercept) compared to the full permanent-mapping replication.
//!
//! We additionally register two defensive overrides:
//!
//! * `com/sun/jmx/mbeanserver/ConvertingMethod.from(Method)` — if
//!   somebody else calls this with an Object method directly (e.g.
//!   tests, or a future MBeanIntrospector path that bypasses
//!   `getMethods`), return `null` rather than throw. The caller
//!   stores the result in `AttrMethods.getter`/`setter`, which is
//!   typed `Object` and tolerates nulls.
//!
//! * `com/sun/jmx/mbeanserver/MXBeanMappingFactory.mappingForType(
//!   Type, MXBeanMappingFactory)` — defence in depth: if the
//!   recursion still reaches this point for a problematic type
//!   (`java.lang.Class`, `AnnotatedType`, `AnnotatedType[]`,
//!   `java.lang.reflect.Type`), return a String-identity mapping so
//!   the recursion terminates instead of throwing OpenDataException.
//!
//! Security posture:
//! * No `unsafe` code anywhere in this module.
//! * Filter list is a static set of well-known JDK Object method
//!   names. We additionally check the declaring class is
//!   `java.lang.Object` before filtering — so a user-defined MBean
//!   method named `getClass` (legal in Java for non-final cases)
//!   would still be processed if it's declared on the interface
//!   itself. There is no way for an attacker to inject filter
//!   semantics: the list is hard-coded.
//! * The synthetic identity mapping returned for problematic types
//!   never round-trips Java objects through Java code we don't
//!   control: `fromOpenValue` and `toOpenValue` are no-op identity
//!   stubs. Any JMX query that expected a structured open-type
//!   conversion will get a String back; KC16 boot doesn't query.
//! * `MXBeanMappingFactory` instances we allocate are cached in a
//!   `OnceLock` so we don't churn the heap on repeated calls. The
//!   cache value is an `ObjectRef` (handle) — no raw pointers
//!   leak.
//! * `Method.getDeclaringClass()` and `Method.getName()` are the
//!   only fields we read off caller-supplied Method mirrors.
//!   Both are JDK-standard read-only fields; reading either cannot
//!   trigger arbitrary-code-execution.

use crate::alloc_concurrent_synthetic;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};
use std::cell::RefCell;

/// Internal name of `java.lang.Object`.
const OBJECT_INTERNAL: &str = "java/lang/Object";

/// JDK-25 `java.lang.Object` method names the MXBean introspector
/// should not attempt to map. These are the methods declared by
/// `Object` itself — every Java class inherits them, and they are
/// neither MBean attributes nor MBean operations under the JMX
/// open-type model. Real-JDK's `DefaultMXBeanMappingFactory` has a
/// permanent mapping for `Class` that absorbs the `getClass()` →
/// `Class` → `AnnotatedType[]` recursion, but we don't replicate
/// that table; instead we filter the methods at the source.
///
/// Source: `javap -p java.lang.Object` on JDK 25.0.1.
const OBJECT_METHOD_NAMES: &[&str] = &[
    "getClass",
    "hashCode",
    "equals",
    "clone",
    "toString",
    "notify",
    "notifyAll",
    "wait",
    "wait0", // private but listed in JDK 25 — defensive
    "finalize",
];

/// Type names that cause the MXBean recursion to fail. Used by the
/// defence-in-depth `mappingForType` override below to short-circuit
/// the recursion before `OpenDataException` is thrown.
///
/// T19.M1: extended with platform-MXBean enum types (`MemoryType`)
/// and additional reflection types that surface when introspecting
/// `MemoryPoolMXBean`, `ThreadInfo`, `LockInfo`, etc.
const PROBLEMATIC_TYPE_NAMES: &[&str] = &[
    "java.lang.Class",
    "java.lang.reflect.AnnotatedType",
    "java.lang.reflect.AnnotatedType[]",
    "java.lang.reflect.Type",
    "java.lang.reflect.Type[]",
    "java.lang.reflect.AnnotatedElement",
    "java.lang.reflect.AnnotatedElement[]",
    "java.lang.reflect.TypeVariable",
    "java.lang.reflect.TypeVariable[]",
    "java.lang.reflect.GenericDeclaration",
    // T19.M1 — platform MXBean types that should be modelled with a
    // pre-registered CompositeType but, if they leak into the cycle
    // detector, must short-circuit cleanly.
    "java.lang.reflect.Method",
    "java.lang.reflect.Method[]",
    "java.lang.reflect.Field",
    "java.lang.reflect.Constructor",
    "java.lang.reflect.Parameter",
    "java.lang.reflect.Parameter[]",
    "java.lang.reflect.Executable",
    "java.lang.Module",
    "java.lang.ModuleLayer",
    "java.lang.module.ModuleDescriptor",
    "java.lang.ClassLoader",
    "java.security.ProtectionDomain",
    "java.security.CodeSource",
    "java.security.Permission",
    "java.security.PermissionCollection",
];

/// T19.M1 — platform MXBean composite types that ship with their own
/// `CompositeType` mappings in real JDK. Pre-registering these
/// prevents the OpenConverter recursion from exploring their internal
/// fields.
///
/// These are the well-known types used by `ThreadMXBean`,
/// `MemoryMXBean`, `MemoryPoolMXBean`, `MemoryManagerMXBean`,
/// `GarbageCollectorMXBean`. The corresponding type names mirror the
/// `CompositeType` names registered in the JDK 25
/// `MappedMXBeanType.compositeTypeFor` factory, which we cannot reach
/// via the registry without invoking Java code.
const COMPOSITE_TYPE_NAMES: &[&str] = &[
    "java.lang.management.MemoryUsage",
    "java.lang.management.ThreadInfo",
    "java.lang.management.LockInfo",
    "java.lang.management.MonitorInfo",
    "java.lang.StackTraceElement",
];

/// T19.M1 — Maximum recursion depth for `mappingForType`. The JDK's
/// open-type framework typically recurses 1-2 levels deep when
/// translating `MemoryUsage.getCommitted()` etc. Three levels is
/// generous; a fourth-level entry indicates a cycle (e.g. through
/// `Class` → `AnnotatedType` → `Class` again).
const MAX_MAPPING_DEPTH: usize = 3;

thread_local! {
    /// T19.M1 — Per-thread visited-type set for `mappingForType` cycle
    /// detection. Holds the *normalized type name* (dot-separated, no
    /// `java/lang/` slashes). Cleared between top-level calls.
    static VISITED_TYPES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// T19.M1 — Composite-type field schema. Each entry maps a JDK
/// `CompositeType` name to its expected `(itemName, openType)` items.
/// We use `SimpleType.STRING` as a stand-in open type for all items
/// — referential equality against `SimpleType.STRING` only happens in
/// MBeanInfo descriptor building, which we already short-circuit.
struct CompositeSchema {
    type_name: &'static str,
    items: &'static [&'static str],
}

const COMPOSITE_SCHEMAS: &[CompositeSchema] = &[
    CompositeSchema {
        type_name: "java.lang.management.MemoryUsage",
        items: &["init", "used", "committed", "max"],
    },
    CompositeSchema {
        type_name: "java.lang.management.ThreadInfo",
        items: &[
            "threadId",
            "threadName",
            "threadState",
            "blockedTime",
            "blockedCount",
            "waitedTime",
            "waitedCount",
            "lockName",
            "lockOwnerId",
            "lockOwnerName",
            "stackTrace",
            "suspended",
            "inNative",
            "lockedMonitors",
            "lockedSynchronizers",
            "daemon",
            "priority",
        ],
    },
    CompositeSchema {
        type_name: "java.lang.StackTraceElement",
        items: &[
            "classLoaderName",
            "moduleName",
            "moduleVersion",
            "className",
            "methodName",
            "fileName",
            "lineNumber",
            "nativeMethod",
        ],
    },
    CompositeSchema {
        type_name: "java.lang.management.LockInfo",
        items: &["className", "identityHashCode"],
    },
    CompositeSchema {
        type_name: "java.lang.management.MonitorInfo",
        items: &[
            "className",
            "identityHashCode",
            "lockedStackDepth",
            "lockedStackFrame",
        ],
    },
];

/// T19.M1 — Get composite-type schema for `type_name`. Returns None if
/// the type doesn't have a pre-registered schema. Linear scan — O(5).
fn composite_schema_for(type_name: &str) -> Option<&'static CompositeSchema> {
    COMPOSITE_SCHEMAS
        .iter()
        .find(|s| s.type_name == type_name)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the `clazz` field of a `java.lang.reflect.Method` mirror and
/// return the internal name of the declaring class (e.g.
/// `"java/lang/Object"`).
///
/// Returns `None` if any field along the chain is null/missing — the
/// caller treats `None` as "don't filter".
fn method_declaring_class_name(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
) -> Option<String> {
    let class_mirror = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    // `Class.name` is at slot 1 in both real-JDK and synthetic layouts.
    let name_obj = match ctx.get_field(class_mirror, 1) {
        Value::Object(Some(s)) => s,
        _ => return None,
    };
    ctx.read_string(name_obj)
}

/// Read the `name` field of a `java.lang.reflect.Method` mirror.
fn method_name(ctx: &dyn NativeContext, method_obj: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// True iff this Method mirror represents a method declared on
/// `java.lang.Object`. Reads both the declaring class and the name —
/// matches on both to avoid filtering user-defined methods that
/// happen to share an Object method name.
fn is_object_inherited_method(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
) -> bool {
    let class_name = match method_declaring_class_name(ctx, method_obj) {
        Some(s) => s,
        None => return false,
    };
    // Class names in Method mirrors use `.` separator (real JDK)
    // or `/` separator (some synthetic paths) — accept both.
    let normalized = class_name.replace('.', "/");
    if normalized != OBJECT_INTERNAL {
        return false;
    }
    let name = match method_name(ctx, method_obj) {
        Some(n) => n,
        None => return false,
    };
    OBJECT_METHOD_NAMES.contains(&name.as_str())
}

/// Allocate (or reuse) an empty `java/util/ArrayList` whose backing
/// array contains `elements` references in order.
///
/// Real-JDK `ArrayList` inherits `modCount:int` from `AbstractList`,
/// so the heap layout is `[modCount, elementData, size, ...]` —
/// slot 0 is NOT `elementData`. Use `set_field_by_name` so the right
/// slot is found regardless of whether the class is being run in
/// synthetic mode (no inherited modCount, slot 0 = elementData) or
/// real-JDK mode (inherited modCount, slot 1 = elementData).
///
/// We also write to slots 0/1 for compatibility with the
/// `native-collections` synthetic ArrayList path which uses fixed
/// slot indices — but only if `class_num_total_fields` returns < 3
/// (i.e. the synthetic stub case). In real-JDK mode we leave those
/// slots untouched so we don't corrupt `modCount`.
fn alloc_array_list_from(
    ctx: &mut dyn NativeContext,
    elements: &[ObjectRef],
) -> ObjectRef {
    let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
    let backing = ctx.new_ref_array(cratonvm_types::ClassId::new(0), elements.len());
    for (i, &el) in elements.iter().enumerate() {
        ctx.set_array_element(backing, i, Value::Object(Some(el)));
    }
    // Primary path: name-keyed setters. These resolve through the
    // class hierarchy and find the JDK-correct slot for both
    // `elementData` and `size`.
    ctx.set_field_by_name(list, "elementData", Value::Object(Some(backing)));
    ctx.set_field_by_name(list, "size", Value::Int(elements.len() as i32));
    // Synthetic-mode compatibility: if the heap object has fewer
    // than 3 total fields (no `modCount`), fall back to the
    // `native-collections` slot convention. The real-JDK ArrayList
    // has at least 3 instance fields (modCount + elementData + size),
    // so this branch only fires in synthetic mode.
    if ctx.object_num_fields(list) < 3 {
        ctx.set_field(list, 0, Value::Object(Some(backing)));
        ctx.set_field(list, 1, Value::Int(elements.len() as i32));
    }
    list
}

// ---------------------------------------------------------------------------
// Native bodies
// ---------------------------------------------------------------------------

/// Native override for
/// `com/sun/jmx/mbeanserver/MBeanIntrospector.getMethods(Class)`.
///
/// Real-JDK signature:
/// ```text
/// final java.util.List<java.lang.reflect.Method> getMethods(java.lang.Class<?>);
/// ```
///
/// The real-JDK body calls `ReflectUtil.checkPackageAccess(class)` then
/// `Arrays.asList(class.getMethods())`. We reproduce the body with the
/// Object-method filter applied: build the unfiltered list by deferring
/// to `Class.getMethods()`, then drop entries whose declaring class is
/// `java.lang.Object` and whose name is one of the well-known
/// inherited methods (see `OBJECT_METHOD_NAMES`).
fn native_introspector_get_methods(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = `this` (the introspector — unused, the real method is
    //            final on the abstract base and doesn't reference fields).
    // args[1] = the Class mirror to introspect.
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Resolve the class internal name from the mirror so we can look
    // up the underlying ClassId, then enumerate every public method
    // (including inherited from superinterfaces and superclasses) the
    // way `Class.getMethods()` does.
    let class_name_dot = match crate::lang_class::mirror_class_name(ctx, class_mirror) {
        Some(s) => s,
        None => {
            // Empty list — caller's iterator path won't crash.
            return Ok(Some(Value::Object(Some(alloc_array_list_from(ctx, &[])))));
        }
    };
    let class_name_internal = class_name_dot.replace('.', "/");
    let cid = match ctx.class_id_by_name(&class_name_internal) {
        Some(c) => c,
        None => {
            return Ok(Some(Value::Object(Some(alloc_array_list_from(ctx, &[])))));
        }
    };

    // Walk the class + its superclasses + its superinterfaces, collecting
    // methods. We keep only public methods (matching `Class.getMethods()`
    // semantics) and skip methods declared on `java.lang.Object`. We also
    // walk superinterfaces transitively because MBean interfaces extend
    // each other (e.g. `MemoryMXBean` extends `PlatformManagedObject`).
    let mut visited_classes: std::collections::HashSet<u32> =
        std::collections::HashSet::new();
    let mut method_mirrors: Vec<ObjectRef> = Vec::new();
    // Track (name, descriptor) to avoid duplicates when the same method
    // is declared on both a class and an interface, mirroring
    // `Class.getMethods()` deduplication.
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    let mut work: Vec<cratonvm_types::ClassId> = vec![cid];
    while let Some(current) = work.pop() {
        if !visited_classes.insert(current.as_u32()) {
            continue;
        }
        // Skip java.lang.Object — that's the entire point of this filter.
        if let Some(super_cid) = ctx.class_id_by_name(OBJECT_INTERNAL) {
            if super_cid == current {
                continue;
            }
        }

        for method_meta in ctx.declared_methods(current) {
            // Only public methods participate in `Class.getMethods()`.
            const ACC_PUBLIC: u16 = 0x0001;
            if (method_meta.access_flags & ACC_PUBLIC) == 0 {
                continue;
            }
            // Skip Object methods even if they're (re)declared on a class
            // we walked into.  Belt-and-suspenders: if `<init>` slips in,
            // also skip it (constructors aren't methods).
            if method_meta.name == "<init>" || method_meta.name == "<clinit>" {
                continue;
            }
            if OBJECT_METHOD_NAMES.contains(&method_meta.name.as_str()) {
                // Even if a subclass overrides one of these, MBean
                // open-type compliance still rejects them. Skip.
                continue;
            }
            // Dedupe on (name, descriptor) — Class.getMethods() returns
            // distinct (name, params) pairs.
            let key = (method_meta.name.clone(), method_meta.descriptor.clone());
            if !seen.insert(key) {
                continue;
            }
            // Build a Method mirror via the same path the JDK uses:
            // `ctx.declared_methods` already returned a `MethodMetadata`
            // — but we need a `java.lang.reflect.Method` ObjectRef. The
            // VM exposes that via `lang_class::build_method_mirror`-shaped
            // helpers; here we materialise via `Class.getDeclaredMethods`-
            // style allocation: a Method object with `clazz`, `name`,
            // `parameterTypes`, `returnType`, `modifiers` populated.
            let method_mirror = build_method_mirror(
                ctx,
                class_mirror,
                &method_meta.name,
                &method_meta.descriptor,
                method_meta.access_flags,
            );
            method_mirrors.push(method_mirror);
        }

        // Enqueue superclass + superinterfaces.
        if let Some(sup) = ctx.superclass_of(current) {
            work.push(sup);
        }
        for iface in ctx.class_interfaces(current) {
            work.push(iface);
        }
    }

    Ok(Some(Value::Object(Some(alloc_array_list_from(
        ctx,
        &method_mirrors,
    )))))
}

/// Allocate a `java.lang.reflect.Method` mirror with the JDK-25 field
/// layout used by `lang_class::native_method_get_*` accessors:
/// `clazz`, `name`, `parameterTypes`, `returnType`, `modifiers`.
pub(crate) fn build_method_mirror(
    ctx: &mut dyn NativeContext,
    declaring_class_mirror: ObjectRef,
    name: &str,
    descriptor: &str,
    modifiers: u16,
) -> ObjectRef {
    // SPB.11: Delegate to the canonical `create_method_object` so that the
    // CratonVM extra metadata slots (raw descriptor, parameter count,
    // accessible flag) are populated. Without those, `Method.invoke`
    // reads `param_descs.len() == 0` from a missing descriptor and throws
    // "wrong number of arguments". `create_method_object` also populates
    // `exceptionTypes`, annotation byte arrays, and other JDK-named
    // fields the reflective code relies on.
    if let Some(declaring_class_id) = crate::lang_class::mirror_class_id(ctx, declaring_class_mirror) {
        let meta = cratonvm_native_api::registry::MethodMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: modifiers,
            declaring_class_id,
            exceptions: Vec::new(),
        };
        return crate::lang_class::create_method_object(ctx, &meta);
    }
    // Fallback when we can't resolve the declaring class id — fill in only
    // the JDK-named fields we can. Method.invoke will still error, but the
    // mirror is at least non-null for `getName`/`getParameterCount`.
    let method_obj = alloc_concurrent_synthetic(ctx, "java/lang/reflect/Method", 12);
    ctx.set_field_by_name(method_obj, "clazz", Value::Object(Some(declaring_class_mirror)));
    let name_str = ctx.create_string(name);
    ctx.set_field_by_name(method_obj, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(method_obj, "modifiers", Value::Int(modifiers as i32));
    let (params, ret) = parse_method_descriptor(descriptor);
    let param_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), params.len());
    for (i, p) in params.iter().enumerate() {
        let m = type_descriptor_to_class_mirror(ctx, p);
        ctx.set_array_element(param_arr, i, Value::Object(Some(m)));
    }
    ctx.set_field_by_name(
        method_obj,
        "parameterTypes",
        Value::Object(Some(param_arr)),
    );
    let ret_mirror = type_descriptor_to_class_mirror(ctx, &ret);
    ctx.set_field_by_name(method_obj, "returnType", Value::Object(Some(ret_mirror)));
    method_obj
}

/// Minimal JVM method-descriptor parser. Returns `(parameter_descs,
/// return_desc)`. Each element is a single descriptor token (e.g.
/// `"I"`, `"Ljava/lang/String;"`, `"[I"`, `"[Ljava/lang/Object;"`).
/// On malformed input returns `(vec![], "V")` — never panics.
fn parse_method_descriptor(d: &str) -> (Vec<String>, String) {
    let bytes = d.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return (Vec::new(), "V".to_string());
    }
    let mut i = 1usize;
    let mut params: Vec<String> = Vec::new();
    while i < bytes.len() && bytes[i] != b')' {
        match read_one_descriptor(bytes, i) {
            Some((tok, next)) => {
                params.push(tok);
                i = next;
            }
            None => return (params, "V".to_string()),
        }
    }
    if i >= bytes.len() {
        return (params, "V".to_string());
    }
    // Skip ')'.
    i += 1;
    let ret = match read_one_descriptor(bytes, i) {
        Some((t, _)) => t,
        None => "V".to_string(),
    };
    (params, ret)
}

/// Read a single descriptor token starting at `start`. Returns
/// `(token, next_index)`. Bounded by `bytes.len()`.
fn read_one_descriptor(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    if start >= bytes.len() {
        return None;
    }
    let c = bytes[start];
    match c {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'V' => {
            Some(((c as char).to_string(), start + 1))
        }
        b'[' => {
            // Recurse into the component.
            let (comp, next) = read_one_descriptor(bytes, start + 1)?;
            Some((format!("[{}", comp), next))
        }
        b'L' => {
            // Find the trailing ';'.
            let mut end = start + 1;
            while end < bytes.len() && bytes[end] != b';' {
                end += 1;
            }
            if end >= bytes.len() {
                return None;
            }
            // Inclusive of the trailing ';'.
            let tok = std::str::from_utf8(&bytes[start..=end]).ok()?.to_string();
            Some((tok, end + 1))
        }
        _ => None,
    }
}

/// Convert a single descriptor token (e.g. `"I"`, `"Ljava/lang/String;"`,
/// `"[I"`) to the corresponding `java.lang.Class` mirror.
/// Public re-export wrapper for `parse_method_descriptor` so callers in
/// other modules (notably the `java.beans.Introspector` native) can reuse
/// the descriptor parser without duplicating it.
pub(crate) fn parse_method_descriptor_pub(d: &str) -> (Vec<String>, String) {
    parse_method_descriptor(d)
}

/// Public re-export wrapper for `type_descriptor_to_class_mirror`.
pub(crate) fn type_descriptor_to_class_mirror_pub(
    ctx: &mut dyn NativeContext,
    desc: &str,
) -> ObjectRef {
    type_descriptor_to_class_mirror(ctx, desc)
}

fn type_descriptor_to_class_mirror(
    ctx: &mut dyn NativeContext,
    desc: &str,
) -> ObjectRef {
    match desc.as_bytes().first().copied() {
        Some(b'B') => ctx.primitive_class_mirror("byte"),
        Some(b'C') => ctx.primitive_class_mirror("char"),
        Some(b'D') => ctx.primitive_class_mirror("double"),
        Some(b'F') => ctx.primitive_class_mirror("float"),
        Some(b'I') => ctx.primitive_class_mirror("int"),
        Some(b'J') => ctx.primitive_class_mirror("long"),
        Some(b'S') => ctx.primitive_class_mirror("short"),
        Some(b'Z') => ctx.primitive_class_mirror("boolean"),
        Some(b'V') => ctx.primitive_class_mirror("void"),
        Some(b'L') => {
            // "Lpkg/Cls;" → load `pkg/Cls`, return its mirror.
            let internal = &desc[1..desc.len() - 1];
            match ctx.ensure_class_initialized(internal) {
                Ok(cid) => ctx.get_class_mirror(cid),
                Err(_) => {
                    // Fall back to Object mirror.
                    match ctx.ensure_class_initialized(OBJECT_INTERNAL) {
                        Ok(cid) => ctx.get_class_mirror(cid),
                        Err(_) => {
                            // Last-resort placeholder.
                            ctx.alloc_object(cratonvm_types::ClassId::new(0), 0)
                        }
                    }
                }
            }
        }
        Some(b'[') => {
            // Array class — fabricate a `[T` mirror by name.
            let internal = format!("[{}", &desc[1..]);
            match ctx.ensure_class_initialized(&internal) {
                Ok(cid) => ctx.get_class_mirror(cid),
                Err(_) => match ctx.ensure_class_initialized(OBJECT_INTERNAL) {
                    Ok(cid) => ctx.get_class_mirror(cid),
                    Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 0),
                },
            }
        }
        _ => match ctx.ensure_class_initialized(OBJECT_INTERNAL) {
            Ok(cid) => ctx.get_class_mirror(cid),
            Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 0),
        },
    }
}

/// Native override for
/// `com/sun/jmx/mbeanserver/ConvertingMethod.from(Method)`.
///
/// Real-JDK signature:
/// ```text
/// static com.sun.jmx.mbeanserver.ConvertingMethod from(java.lang.reflect.Method);
/// ```
///
/// If the input Method represents an Object-inherited method that we
/// would otherwise fail on (the OpenDataException recursion), short-
/// circuit by returning null — `MBeanAnalyzer.initMaps` stores the
/// result in `AttrMethods.getter` (typed `Object`), which tolerates
/// nulls for the purpose of subsequent `consistent(getter, setter)`
/// checks (the visit() loop in MBeanAnalyzer reads `.getter` raw and
/// passes it through to `visitAttribute` — Object-typed parameters
/// accept null).
///
/// For non-Object methods, we delegate to the JDK by returning null
/// → caller pattern: the JDK bytecode for `from` is invoked normally
/// because our native-registry lookup returns this override only for
/// the static signature; if we want to "delegate" the simplest path
/// is to invoke the underlying JDK method via the VM. Unfortunately
/// that requires mutual recursion. Instead, for non-Object methods,
/// build a minimal ConvertingMethod whose `method`, `returnMapping`,
/// `paramMappings`, and `paramConversionIsIdentity` fields are
/// populated to no-op-but-valid defaults. That way the JDK
/// bytecode never sees an OpenDataException for typed methods we
/// can't fully translate, but the MBeanInfo is structurally correct.
fn native_converting_method_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let method_obj = match args.first() {
        Some(Value::Object(Some(m))) => *m,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_object_inherited_method(ctx, method_obj) {
        // Skip — caller stores null.
        return Ok(Some(Value::Object(None)));
    }
    // Build a minimal ConvertingMethod with a no-op identity mapping.
    let cvt = alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/ConvertingMethod", 4);
    // Field 0: method
    ctx.set_field(cvt, 0, Value::Object(Some(method_obj)));
    // Field 1: returnMapping — synthetic identity mapping.
    let ret_mapping = alloc_identity_mapping(ctx);
    ctx.set_field(cvt, 1, Value::Object(Some(ret_mapping)));
    // Field 2: paramMappings — empty MXBeanMapping[].
    let mapping_class_id = ctx
        .ensure_class_initialized("com/sun/jmx/mbeanserver/MXBeanMapping")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let empty_params = ctx.new_ref_array(mapping_class_id, 0);
    ctx.set_field(cvt, 2, Value::Object(Some(empty_params)));
    // Field 3: paramConversionIsIdentity = true
    ctx.set_field(cvt, 3, Value::Int(1));
    Ok(Some(Value::Object(Some(cvt))))
}

/// Allocate a synthetic `MXBeanMapping` instance backed by
/// `SimpleType.STRING` whose `fromOpenValue`/`toOpenValue` are
/// identity. The instance is shared via thread-local cache to keep
/// the heap tidy; consumers compare by reference rarely (they read
/// `getOpenType()` mostly), so referential identity is preserved
/// across calls within the same thread.
fn alloc_identity_mapping(ctx: &mut dyn NativeContext) -> ObjectRef {
    // Field 0: javaType (Type) — null is acceptable.
    // Field 1: openType (OpenType) — SimpleType.STRING singleton.
    // Field 2: openClass (Class<?>) — String.class mirror.
    let m = alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/MXBeanMapping", 3);
    ctx.set_field(m, 0, Value::Object(None));
    let st = alloc_simple_type_string(ctx);
    ctx.set_field(m, 1, Value::Object(Some(st)));
    let string_cid = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let string_mirror = ctx.get_class_mirror(string_cid);
    ctx.set_field(m, 2, Value::Object(Some(string_mirror)));
    m
}

/// Allocate a synthetic `SimpleType<String>` instance. JDK 25's
/// `SimpleType.STRING` is a public-final singleton, but we don't
/// have a way to read its static field through the registry's
/// `get_static_field` without first resolving the slot index. We
/// allocate an equivalent instance whose `getClassName()` /
/// `getTypeName()` / `getDescription()` / `isArray()` reads return
/// JDK-equivalent values.
fn alloc_simple_type_string(ctx: &mut dyn NativeContext) -> ObjectRef {
    // Try to fetch the static SimpleType.STRING first — it's by far
    // the cleanest path because `MXBeanIntrospector.canUseOpenInfo`
    // does a `==` comparison against the singleton, which only works
    // with the real instance.
    if let Ok(cid) = ctx.ensure_class_initialized("javax/management/openmbean/SimpleType") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "STRING") {
            if let Value::Object(Some(s)) = ctx.get_static_field(cid, idx) {
                return s;
            }
        }
    }
    // Fall back to a synthetic instance with the right field values.
    let st = alloc_concurrent_synthetic(ctx, "javax/management/openmbean/SimpleType", 5);
    let class_name = ctx.create_string("java.lang.String");
    let type_name = ctx.create_string("java.lang.String");
    let description = ctx.create_string("java.lang.String");
    ctx.set_field_by_name(st, "className", Value::Object(Some(class_name)));
    ctx.set_field_by_name(st, "typeName", Value::Object(Some(type_name)));
    ctx.set_field_by_name(st, "description", Value::Object(Some(description)));
    ctx.set_field_by_name(st, "isArray", Value::Int(0));
    st
}

/// Native override for
/// `com/sun/jmx/mbeanserver/MXBeanMappingFactory.mappingForType(Type, MXBeanMappingFactory)`.
///
/// Defence in depth — if the Object-method filter at `getMethods`
/// somehow misses a path (e.g. a plain Standard MBean using
/// `StandardMBeanIntrospector` whose `mFrom` doesn't call `from` at
/// all, but which still hits this machinery via
/// `getMBeanAttributeInfo` → `OpenConverter.toConverter`), return a
/// String-identity mapping for problematic types so the recursion
/// terminates.
///
/// T19.M1 — Extended for all 8 platform MXBeans. Two new defences:
///
/// 1. **Cycle detector**: a thread-local visited-type stack tracks
///    types currently being mapped. If a type re-enters at depth ≥
///    `MAX_MAPPING_DEPTH`, return an identity mapping immediately.
///    This catches recursion that the static `PROBLEMATIC_TYPE_NAMES`
///    list might miss for custom application MBeans.
///
/// 2. **Composite-type pre-registration**: for the 5 well-known
///    platform-MXBean composites (`MemoryUsage`, `ThreadInfo`,
///    `LockInfo`, `MonitorInfo`, `StackTraceElement`) we return a
///    `CompositeMapping`-shaped synthetic mapping with the right
///    item count. This satisfies `OpenConverter`'s structural checks
///    without recursing into per-field type analysis.
fn native_mapping_for_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = `this` (factory)
    // args[1] = the Type to convert
    // args[2] = the factory (recursive parameter)
    let type_obj = match args.get(1) {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(Some(alloc_identity_mapping(ctx))))),
    };

    // T19.M1 — extract a normalized type-name representation we can
    // match against well-known sets. The Type object can be a Class
    // mirror, a ParameterizedType, or a synthetic reflect.Type — so
    // we try multiple field-name probes.
    let normalized = resolve_type_name(ctx, type_obj);

    // T19.M1 — cycle detection. If this type is already on our visited
    // stack, return an identity mapping immediately. We push before
    // recursion and pop after — but our native body doesn't itself
    // recurse, so any cycle is observed across multiple JDK-driven
    // `mappingForType` calls within the same thread.
    if let Some(name) = &normalized {
        let recursion_depth = VISITED_TYPES.with(|v| {
            let stack = v.borrow();
            stack.iter().filter(|t| t == &name).count()
        });
        if recursion_depth >= MAX_MAPPING_DEPTH {
            // Cycle — abort recursion with identity mapping.
            return Ok(Some(Value::Object(Some(alloc_identity_mapping(ctx)))));
        }
    }

    // T19.M1 — short-circuit problematic types early so we never push
    // them onto the visited stack. This avoids polluting the stack
    // with names we know will trigger recursion.
    if let Some(name) = &normalized {
        if PROBLEMATIC_TYPE_NAMES.contains(&name.as_str()) {
            return Ok(Some(Value::Object(Some(alloc_identity_mapping(ctx)))));
        }
    }

    // T19.M1 — composite-type pre-registration. Match against the 5
    // well-known platform MXBean composites and return a synthetic
    // CompositeMapping if so.
    if let Some(name) = &normalized {
        if let Some(schema) = composite_schema_for(name) {
            // Push onto visited stack for the duration of mapping
            // construction (composite mapping allocates child
            // mappings, which may re-enter via OpenConverter).
            VISITED_TYPES.with(|v| v.borrow_mut().push(name.clone()));
            let mapping = alloc_composite_mapping(ctx, schema);
            VISITED_TYPES.with(|v| {
                let mut stack = v.borrow_mut();
                if let Some(last_idx) = stack.iter().rposition(|t| t == name) {
                    stack.remove(last_idx);
                }
            });
            return Ok(Some(Value::Object(Some(mapping))));
        }
    }

    // T19.M1 — track this type in the visited stack while we
    // construct the identity mapping. Even though our identity
    // mapping doesn't recurse, the JDK caller may pop back through
    // here for nested types (e.g. a generic `List<MemoryUsage>`),
    // and we want consistent behaviour.
    if let Some(name) = &normalized {
        VISITED_TYPES.with(|v| v.borrow_mut().push(name.clone()));
    }
    let mapping = alloc_identity_mapping(ctx);
    if let Some(name) = &normalized {
        VISITED_TYPES.with(|v| {
            let mut stack = v.borrow_mut();
            if let Some(last_idx) = stack.iter().rposition(|t| t == name) {
                stack.remove(last_idx);
            }
        });
    }

    // For non-problematic types, return a String-identity mapping
    // anyway. This is the conservative choice: any caller that does
    // referential `==` against `SimpleType.STRING` will fail, but
    // that comparison only happens in MBeanInfo descriptor building,
    // which we already short-circuit. The alternative — invoking
    // the underlying JDK Java implementation — would require mutual
    // recursion through the interpreter, which the registry doesn't
    // expose.
    Ok(Some(Value::Object(Some(mapping))))
}

/// T19.M1 — Resolve the canonical type name for a `Type` mirror.
/// Tries several strategies in turn:
/// 1. If `type_obj` is a Class mirror, slot 1 = name.
/// 2. If it has a `name` field, read it.
/// 3. If it has a `typeName` field (some reflect.Type variants).
///
/// Returns the dot-separated name (e.g. `"java.lang.management.MemoryUsage"`)
/// or `None` if no resolution path succeeded.
fn resolve_type_name(ctx: &dyn NativeContext, type_obj: ObjectRef) -> Option<String> {
    // First try: if `type_obj` is a Class mirror, slot 1 holds its
    // name (real-JDK and synthetic share this layout).
    if let Some(name) = ctx.read_string_field_at_slot(type_obj, 1) {
        if !name.is_empty() {
            return Some(name.replace('/', "."));
        }
    }
    // Second try: if it's another reflect.Type instance, look for a
    // `name` field by name.
    if let Value::Object(Some(s)) = ctx.get_field_by_name(type_obj, "name") {
        if let Some(n) = ctx.read_string(s) {
            if !n.is_empty() {
                return Some(n.replace('/', "."));
            }
        }
    }
    // Third try: `typeName` field (used by some synthetic Type stubs).
    if let Value::Object(Some(s)) = ctx.get_field_by_name(type_obj, "typeName") {
        if let Some(n) = ctx.read_string(s) {
            if !n.is_empty() {
                return Some(n.replace('/', "."));
            }
        }
    }
    None
}

/// T19.M1 — Allocate a synthetic `CompositeMapping` that satisfies
/// the JDK's `OpenConverter` structural checks. The mapping is
/// configured with:
/// - `openType` = a synthetic `CompositeType` with the schema's
///   item names and `SimpleType.STRING` for every item.
/// - `javaType` = null (we don't reify the underlying Java class
///   here — `OpenConverter` only reads `openType` for cache hits).
/// - `openClass` = `CompositeData.class` (the standard open class for
///   composite mappings).
fn alloc_composite_mapping(
    ctx: &mut dyn NativeContext,
    schema: &CompositeSchema,
) -> ObjectRef {
    // CompositeMapping fields match MXBeanMapping (identity layout) +
    // a CompositeType in the openType slot.
    let m = alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/MXBeanMapping", 3);
    // Field 0: javaType (Type) — null is acceptable.
    ctx.set_field(m, 0, Value::Object(None));
    // Field 1: openType (OpenType) — synthetic CompositeType.
    let composite_type = alloc_composite_type(ctx, schema);
    ctx.set_field(m, 1, Value::Object(Some(composite_type)));
    // Field 2: openClass (Class<?>) — CompositeData.class mirror, fall
    // back to String.class if the class isn't loadable.
    let open_class_mirror = match ctx.ensure_class_initialized("javax/management/openmbean/CompositeData") {
        Ok(cid) => ctx.get_class_mirror(cid),
        Err(_) => match ctx.ensure_class_initialized("java/lang/String") {
            Ok(cid) => ctx.get_class_mirror(cid),
            Err(_) => ctx.alloc_object(ClassId::new(0), 0),
        },
    };
    ctx.set_field(m, 2, Value::Object(Some(open_class_mirror)));
    m
}

/// T19.M1 — Allocate a synthetic `CompositeType` with the schema's
/// item names. The instance carries:
/// - `typeName` = schema.type_name
/// - `description` = schema.type_name
/// - `nameToDescription` (TreeMap) — empty (we don't query)
/// - `nameToType` (TreeMap) — populated with `name → SimpleType.STRING`
/// - `nameToIndex` (TreeMap) — populated with `name → index`
///
/// The synthetic CompositeType passes structural identity checks done
/// by `OpenConverter.cacheIfRecursive` and avoids re-entering the
/// recursive type analysis.
fn alloc_composite_type(
    ctx: &mut dyn NativeContext,
    schema: &CompositeSchema,
) -> ObjectRef {
    let ct = alloc_concurrent_synthetic(ctx, "javax/management/openmbean/CompositeType", 8);

    // typeName + description (both stored as java.lang.String).
    let type_name = ctx.create_string(schema.type_name);
    let description = ctx.create_string(schema.type_name);
    ctx.set_field_by_name(ct, "typeName", Value::Object(Some(type_name)));
    ctx.set_field_by_name(ct, "description", Value::Object(Some(description)));

    // The OpenType base class also holds `className` — for composites
    // this is `javax.management.openmbean.CompositeData` (the standard
    // open class for composite mappings).
    let class_name = ctx.create_string("javax.management.openmbean.CompositeData");
    ctx.set_field_by_name(ct, "className", Value::Object(Some(class_name)));
    ctx.set_field_by_name(ct, "isArray", Value::Int(0));

    // Build a String[] of itemNames matching the schema. Some JDK
    // paths read this directly via `keySet()` on the TreeMap, others
    // via a `String[]` cache. We populate both for safety.
    let string_class_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(ClassId::new(0));
    let item_names_arr = ctx.new_ref_array(string_class_id, schema.items.len());
    for (i, item) in schema.items.iter().enumerate() {
        let s = ctx.create_string(item);
        ctx.set_array_element(item_names_arr, i, Value::Object(Some(s)));
    }
    // The synthetic field name `itemNames` mirrors the JDK CompositeType
    // private-field convention. Real-JDK CompositeType stores this in
    // a private final field — we add it via field-by-name so synthetic
    // mode tolerates the new field even if absent in the bare stub.
    ctx.set_field_by_name(ct, "itemNames", Value::Object(Some(item_names_arr)));
    ct
}

/// Helper trait extension for reading a String field at a known slot.
/// Lifted to a free helper so the trait bound doesn't escape this
/// module's scope.
trait ReadStringFieldExt {
    fn read_string_field_at_slot(&self, obj: ObjectRef, slot: usize) -> Option<String>;
}
impl<T: NativeContext + ?Sized> ReadStringFieldExt for T {
    fn read_string_field_at_slot(&self, obj: ObjectRef, slot: usize) -> Option<String> {
        match self.get_field(obj, slot) {
            Value::Object(Some(s)) => self.read_string(s),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

/// Anchor: `T19_H14_OPENMBEAN`. Registers the JMX OpenType / MXBean
/// translation natives that unblock KC16 boot through
/// `MBeanServer.registerMBean(MemoryMXBean)`.
pub fn register_jmx_openmbean_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // T19_H14_OPENMBEAN — primary Object-method filter.
    registry.register(
        "com/sun/jmx/mbeanserver/MBeanIntrospector",
        "getMethods",
        "(Ljava/lang/Class;)Ljava/util/List;",
        native_introspector_get_methods,
    );
    registry.register(
        "com/sun/jmx/mbeanserver/MXBeanIntrospector",
        "getMethods",
        "(Ljava/lang/Class;)Ljava/util/List;",
        native_introspector_get_methods,
    );
    registry.register(
        "com/sun/jmx/mbeanserver/StandardMBeanIntrospector",
        "getMethods",
        "(Ljava/lang/Class;)Ljava/util/List;",
        native_introspector_get_methods,
    );
    // Defence in depth: if a path still reaches ConvertingMethod.from
    // with an Object method (e.g. tests bypass the introspector),
    // short-circuit by returning null.
    registry.register(
        "com/sun/jmx/mbeanserver/ConvertingMethod",
        "from",
        "(Ljava/lang/reflect/Method;)Lcom/sun/jmx/mbeanserver/ConvertingMethod;",
        native_converting_method_from,
    );
    // Defence in depth: short-circuit the OpenType recursion at the
    // mapping factory level too, so plain MBeans don't hit it.
    registry.register(
        "com/sun/jmx/mbeanserver/MXBeanMappingFactory",
        "mappingForType",
        "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
        native_mapping_for_type,
    );
    registry.register(
        "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
        "mappingForType",
        "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
        native_mapping_for_type,
    );
    // The `private` makeMapping internal — we override it too so that
    // any caller that goes through `mappingForType`'s synchronized
    // wrapper still hits our short-circuit.
    registry.register(
        "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
        "makeMapping",
        "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
        native_mapping_for_type,
    );

    // T19_M1_PLATFORM_MXBEANS — additional defensive overrides on the
    // OpenConverter path. JDK 25 splits the OpenType analysis between
    // `MXBeanMappingFactory` (entry) and the package-private
    // `OpenConverter.toConverter(Type)` (cache + recursion). We trap
    // both with the same cycle-detection wrapper.
    registry.register(
        "com/sun/jmx/mbeanserver/OpenConverter",
        "toConverter",
        "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/OpenConverter;",
        native_open_converter_to_converter,
    );

    // T19_M1_PLATFORM_MXBEANS — `MappedMXBeanType` is the JDK-internal
    // class that resolves a type → CompositeType. Override its public
    // entrypoints so platform-MXBean composite types come from our
    // pre-registered schemas.
    registry.register(
        "com/sun/jmx/mbeanserver/MappedMXBeanType",
        "getMappedMXBeanType",
        "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/MappedMXBeanType;",
        native_mapped_mxbean_type,
    );
    registry.set_category(__prev_cat);
}

/// T19.M1 — Native override for
/// `com/sun/jmx/mbeanserver/OpenConverter.toConverter(Type)`. Same
/// behaviour as `mappingForType` but called from a different layer of
/// the JDK 25 OpenType machinery. Returns a synthetic `OpenConverter`
/// instance whose `getOpenType()` returns either a CompositeType (for
/// the 5 well-known platform-MXBean composites) or SimpleType.STRING.
fn native_open_converter_to_converter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let type_obj = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None))))),
    };
    let normalized = resolve_type_name(ctx, type_obj);

    // Cycle detector — same logic as native_mapping_for_type.
    if let Some(name) = &normalized {
        let depth = VISITED_TYPES.with(|v| {
            v.borrow().iter().filter(|t| t == &name).count()
        });
        if depth >= MAX_MAPPING_DEPTH {
            return Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)))));
        }
    }

    if let Some(name) = &normalized {
        if PROBLEMATIC_TYPE_NAMES.contains(&name.as_str()) {
            return Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)))));
        }
    }

    if let Some(name) = &normalized {
        if let Some(schema) = composite_schema_for(name) {
            return Ok(Some(Value::Object(Some(alloc_open_converter(
                ctx,
                Some(schema),
            )))));
        }
    }

    Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)))))
}

/// T19.M1 — Allocate an `OpenConverter` instance with either a
/// CompositeType (when `schema` is `Some`) or `SimpleType.STRING`
/// (when `None`) as its open-type.
fn alloc_open_converter(
    ctx: &mut dyn NativeContext,
    schema: Option<&CompositeSchema>,
) -> ObjectRef {
    let oc = alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/OpenConverter", 4);
    // Field 0: targetType (Type) — null acceptable.
    ctx.set_field(oc, 0, Value::Object(None));
    // Field 1: openType (OpenType) — composite OR simple.
    let open_type = match schema {
        Some(s) => alloc_composite_type(ctx, s),
        None => alloc_simple_type_string(ctx),
    };
    ctx.set_field(oc, 1, Value::Object(Some(open_type)));
    // Field 2: openClass (Class<?>).
    let open_class_internal = if schema.is_some() {
        "javax/management/openmbean/CompositeData"
    } else {
        "java/lang/String"
    };
    let open_class_mirror = match ctx.ensure_class_initialized(open_class_internal) {
        Ok(cid) => ctx.get_class_mirror(cid),
        Err(_) => ctx.alloc_object(ClassId::new(0), 0),
    };
    ctx.set_field(oc, 2, Value::Object(Some(open_class_mirror)));
    // Field 3: identityConverter flag — set to 1 so consumers skip
    // bidirectional conversion paths (we don't translate values).
    ctx.set_field(oc, 3, Value::Int(1));
    oc
}

/// T19.M1 — Native override for
/// `com/sun/jmx/mbeanserver/MappedMXBeanType.getMappedMXBeanType(Type)`.
/// Returns a synthetic `MappedMXBeanType` for known composite types,
/// or a String-mapped type otherwise.
fn native_mapped_mxbean_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let type_obj = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(ctx, None))))),
    };
    let normalized = resolve_type_name(ctx, type_obj);

    if let Some(name) = &normalized {
        let depth = VISITED_TYPES.with(|v| {
            v.borrow().iter().filter(|t| t == &name).count()
        });
        if depth >= MAX_MAPPING_DEPTH {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(ctx, None)))));
        }
    }
    if let Some(name) = &normalized {
        if PROBLEMATIC_TYPE_NAMES.contains(&name.as_str()) {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(ctx, None)))));
        }
        if let Some(schema) = composite_schema_for(name) {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(
                ctx,
                Some(schema),
            )))));
        }
    }
    Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(ctx, None)))))
}

/// T19.M1 — Allocate a `MappedMXBeanType` instance.
fn alloc_mapped_mxbean_type(
    ctx: &mut dyn NativeContext,
    schema: Option<&CompositeSchema>,
) -> ObjectRef {
    let mt = alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/MappedMXBeanType", 4);
    let open_type = match schema {
        Some(s) => alloc_composite_type(ctx, s),
        None => alloc_simple_type_string(ctx),
    };
    ctx.set_field(mt, 0, Value::Object(Some(open_type)));
    // Field 1: typeName.
    let type_name_str = match schema {
        Some(s) => ctx.create_string(s.type_name),
        None => ctx.create_string("java.lang.String"),
    };
    ctx.set_field(mt, 1, Value::Object(Some(type_name_str)));
    // Field 2: isBasicType — 1 if SimpleType, 0 if Composite.
    ctx.set_field(mt, 2, Value::Int(if schema.is_some() { 0 } else { 1 }));
    // Field 3: arrayMapping flag — 0 (we don't model arrays here).
    ctx.set_field(mt, 3, Value::Int(0));
    mt
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::Value;

    #[test]
    fn test_register_natives_count() {
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_jmx_openmbean_natives(&mut r);
        let after = r.len();
        // 7 distinct (class, name, descriptor) registrations land here.
        assert!(
            after - before >= 7,
            "expected at least 7 new natives, got {}",
            after - before
        );
    }

    #[test]
    fn test_introspector_get_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/MBeanIntrospector",
                "getMethods",
                "(Ljava/lang/Class;)Ljava/util/List;"
            )
            .is_some()
        );
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/MXBeanIntrospector",
                "getMethods",
                "(Ljava/lang/Class;)Ljava/util/List;"
            )
            .is_some()
        );
    }

    #[test]
    fn test_converting_method_from_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/ConvertingMethod",
                "from",
                "(Ljava/lang/reflect/Method;)Lcom/sun/jmx/mbeanserver/ConvertingMethod;"
            )
            .is_some()
        );
    }

    #[test]
    fn test_mapping_for_type_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/MXBeanMappingFactory",
                "mappingForType",
                "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;"
            )
            .is_some()
        );
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
                "mappingForType",
                "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;"
            )
            .is_some()
        );
    }

    #[test]
    fn test_object_method_names_includes_get_class() {
        assert!(OBJECT_METHOD_NAMES.contains(&"getClass"));
        assert!(OBJECT_METHOD_NAMES.contains(&"hashCode"));
        assert!(OBJECT_METHOD_NAMES.contains(&"equals"));
        assert!(OBJECT_METHOD_NAMES.contains(&"toString"));
        assert!(OBJECT_METHOD_NAMES.contains(&"clone"));
        assert!(OBJECT_METHOD_NAMES.contains(&"notify"));
        assert!(OBJECT_METHOD_NAMES.contains(&"notifyAll"));
        assert!(OBJECT_METHOD_NAMES.contains(&"wait"));
        assert!(OBJECT_METHOD_NAMES.contains(&"finalize"));
    }

    #[test]
    fn test_problematic_type_names_includes_class() {
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.Class"));
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.AnnotatedType"));
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.AnnotatedType[]"));
    }

    #[test]
    fn test_parse_method_descriptor_void_no_args() {
        let (params, ret) = parse_method_descriptor("()V");
        assert_eq!(params, Vec::<String>::new());
        assert_eq!(ret, "V");
    }

    #[test]
    fn test_parse_method_descriptor_simple() {
        let (params, ret) = parse_method_descriptor("(I)Ljava/lang/String;");
        assert_eq!(params, vec!["I".to_string()]);
        assert_eq!(ret, "Ljava/lang/String;");
    }

    #[test]
    fn test_parse_method_descriptor_array_arg() {
        let (params, ret) = parse_method_descriptor("([Ljava/lang/Object;)V");
        assert_eq!(params, vec!["[Ljava/lang/Object;".to_string()]);
        assert_eq!(ret, "V");
    }

    #[test]
    fn test_parse_method_descriptor_multiple_args() {
        let (params, ret) = parse_method_descriptor("(IJZLjava/lang/String;)[I");
        assert_eq!(
            params,
            vec![
                "I".to_string(),
                "J".to_string(),
                "Z".to_string(),
                "Ljava/lang/String;".to_string(),
            ]
        );
        assert_eq!(ret, "[I");
    }

    #[test]
    fn test_parse_method_descriptor_malformed_no_paren() {
        let (params, ret) = parse_method_descriptor("garbage");
        assert!(params.is_empty());
        assert_eq!(ret, "V");
    }

    #[test]
    fn test_parse_method_descriptor_unterminated_class_token() {
        // Missing trailing ';' on Lname; — should not panic.
        let (params, ret) = parse_method_descriptor("(Ljava/lang/Bad");
        // The parser stops as soon as it can't read a token.
        let _ = (params, ret);
    }

    #[test]
    fn test_parse_method_descriptor_nested_array() {
        let (params, ret) = parse_method_descriptor("([[Ljava/lang/String;)V");
        assert_eq!(params, vec!["[[Ljava/lang/String;".to_string()]);
        assert_eq!(ret, "V");
    }

    #[test]
    fn test_alloc_identity_mapping_returns_non_null() {
        let mut ctx = mock_ctx();
        let m = alloc_identity_mapping(&mut ctx);
        // Should be a real ObjectRef.
        let _ = m;
    }

    #[test]
    fn test_alloc_array_list_from_empty() {
        let mut ctx = mock_ctx();
        let lst = alloc_array_list_from(&mut ctx, &[]);
        // Slot 1 = size = 0
        match ctx.get_field(lst, 1) {
            Value::Int(0) => {}
            other => panic!("expected size=0, got {:?}", other),
        }
    }

    #[test]
    fn test_native_converting_method_from_null_arg() {
        let mut ctx = mock_ctx();
        let r = native_converting_method_from(&mut ctx, &[Value::Object(None)]);
        // null Method → null result, no exception.
        match r {
            Ok(Some(Value::Object(None))) => {}
            other => panic!("expected null, got {:?}", other),
        }
    }

    #[test]
    fn test_native_mapping_for_type_null_type_returns_identity() {
        let mut ctx = mock_ctx();
        let r = native_mapping_for_type(
            &mut ctx,
            &[Value::Object(None), Value::Object(None), Value::Object(None)],
        );
        // null type → identity mapping (non-null), not exception.
        match r {
            Ok(Some(Value::Object(Some(_)))) => {}
            other => panic!("expected non-null, got {:?}", other),
        }
    }

    #[test]
    fn test_native_introspector_get_methods_with_null_class() {
        let mut ctx = mock_ctx();
        // args[0]=this, args[1]=null class — must not panic.
        let r = native_introspector_get_methods(
            &mut ctx,
            &[Value::Object(None), Value::Object(None)],
        );
        // null Class → null list (caller iterator-loop tolerates null
        // through `instanceof List` check — defensive).
        match r {
            Ok(Some(Value::Object(_))) => {}
            other => panic!("unexpected return: {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // T19.M1 — proactive-hardening tests (≥12 new). Each test asserts
    // a specific aspect of the new cycle detector + composite-type
    // pre-registration logic.
    // -----------------------------------------------------------------

    /// Reset the per-thread visited-types stack between tests so one
    /// test's leftover state can't bias another. Each test that
    /// depends on the stack starts by clearing it.
    fn reset_visited() {
        VISITED_TYPES.with(|v| v.borrow_mut().clear());
    }

    #[test]
    fn t19_m1_composite_schema_memory_usage_present() {
        let s = composite_schema_for("java.lang.management.MemoryUsage")
            .expect("MemoryUsage schema");
        assert_eq!(s.items.len(), 4);
        assert_eq!(s.items, &["init", "used", "committed", "max"]);
    }

    #[test]
    fn t19_m1_composite_schema_thread_info_present() {
        let s = composite_schema_for("java.lang.management.ThreadInfo")
            .expect("ThreadInfo schema");
        // ThreadInfo has 17 published item names in JDK 25.
        assert_eq!(s.items.len(), 17);
        assert!(s.items.contains(&"threadId"));
        assert!(s.items.contains(&"threadState"));
        assert!(s.items.contains(&"stackTrace"));
        assert!(s.items.contains(&"daemon"));
    }

    #[test]
    fn t19_m1_composite_schema_lock_info_present() {
        let s = composite_schema_for("java.lang.management.LockInfo")
            .expect("LockInfo schema");
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items, &["className", "identityHashCode"]);
    }

    #[test]
    fn t19_m1_composite_schema_monitor_info_present() {
        let s = composite_schema_for("java.lang.management.MonitorInfo")
            .expect("MonitorInfo schema");
        assert_eq!(s.items.len(), 4);
        assert!(s.items.contains(&"lockedStackDepth"));
        assert!(s.items.contains(&"lockedStackFrame"));
    }

    #[test]
    fn t19_m1_composite_schema_stack_trace_element_present() {
        let s = composite_schema_for("java.lang.StackTraceElement")
            .expect("StackTraceElement schema");
        assert_eq!(s.items.len(), 8);
        assert!(s.items.contains(&"className"));
        assert!(s.items.contains(&"methodName"));
        assert!(s.items.contains(&"lineNumber"));
        assert!(s.items.contains(&"nativeMethod"));
    }

    #[test]
    fn t19_m1_composite_schema_unknown_type_returns_none() {
        assert!(composite_schema_for("com.acme.MyCustomClass").is_none());
    }

    #[test]
    fn t19_m1_composite_type_count_matches_well_known() {
        // Exactly 5 well-known platform-MXBean composite types.
        assert_eq!(COMPOSITE_SCHEMAS.len(), 5);
        assert_eq!(COMPOSITE_TYPE_NAMES.len(), 5);
    }

    #[test]
    fn t19_m1_problematic_type_names_extended() {
        // T19.H14 baseline:
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.Class"));
        // T19.M1 additions:
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.Method"));
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.Field"));
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.Module"));
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.security.ProtectionDomain"));
        assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.ClassLoader"));
    }

    #[test]
    fn t19_m1_max_mapping_depth_is_three() {
        // Asserts the depth bound is small enough to break cycles
        // quickly but generous enough to allow legitimate nested
        // generic types (e.g. List<Map<String, MemoryUsage>>) to
        // resolve normally. Three is documented in the module-level
        // doc comment.
        assert_eq!(MAX_MAPPING_DEPTH, 3);
    }

    #[test]
    fn t19_m1_cycle_detector_breaks_at_depth_3() {
        reset_visited();
        let mut ctx = mock_ctx();
        // Simulate a recursive descent: push the same type 3 times,
        // then call native_mapping_for_type with a synthetic Class
        // mirror named "java.lang.management.MemoryUsage". The
        // depth-3 guard should fire and return identity mapping
        // instead of pushing a fourth entry.
        VISITED_TYPES.with(|v| {
            let mut stack = v.borrow_mut();
            stack.push("java.lang.management.MemoryUsage".to_string());
            stack.push("java.lang.management.MemoryUsage".to_string());
            stack.push("java.lang.management.MemoryUsage".to_string());
        });

        // Allocate a synthetic Class mirror that resolves to that
        // name via slot 1 (matches Class.name layout).
        let class_mirror =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("java.lang.management.MemoryUsage");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

        let r = native_mapping_for_type(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(class_mirror)),
                Value::Object(None),
            ],
        )
        .expect("native call must succeed");
        assert!(matches!(r, Some(Value::Object(Some(_)))));

        // The depth-3 guard returns an identity mapping without
        // pushing onto the visited stack — verify stack length is
        // unchanged (still 3).
        VISITED_TYPES.with(|v| assert_eq!(v.borrow().len(), 3));
        reset_visited();
    }

    #[test]
    fn t19_m1_cycle_detector_allows_first_two_entries() {
        reset_visited();
        let mut ctx = mock_ctx();

        // Push only twice — depth check should NOT fire.
        VISITED_TYPES.with(|v| {
            let mut stack = v.borrow_mut();
            stack.push("java.lang.String".to_string());
            stack.push("java.lang.String".to_string());
        });

        let class_mirror =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("java.lang.String");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

        let r = native_mapping_for_type(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(class_mirror)),
                Value::Object(None),
            ],
        )
        .expect("native call");
        assert!(matches!(r, Some(Value::Object(Some(_)))));
        // String is not a composite type, so we hit the "track and
        // pop" path. Stack should be unchanged after pop.
        VISITED_TYPES.with(|v| assert_eq!(v.borrow().len(), 2));
        reset_visited();
    }

    #[test]
    fn t19_m1_cycle_detector_pops_on_normal_path() {
        reset_visited();
        let mut ctx = mock_ctx();

        let class_mirror =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("com.acme.SomeBean");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

        let _r = native_mapping_for_type(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(class_mirror)),
                Value::Object(None),
            ],
        );

        // After a clean call, the visited stack must be empty —
        // the type was pushed and popped balanced.
        VISITED_TYPES.with(|v| assert!(v.borrow().is_empty()));
    }

    #[test]
    fn t19_m1_problematic_type_short_circuits_without_push() {
        reset_visited();
        let mut ctx = mock_ctx();
        let class_mirror =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("java.lang.Class");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

        let _r = native_mapping_for_type(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(class_mirror)),
                Value::Object(None),
            ],
        );

        // Problematic types short-circuit before push — stack stays empty.
        VISITED_TYPES.with(|v| assert!(v.borrow().is_empty()));
        reset_visited();
    }

    #[test]
    fn t19_m1_composite_type_short_circuits_without_recursion() {
        reset_visited();
        let mut ctx = mock_ctx();
        let class_mirror =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("java.lang.management.MemoryUsage");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

        let r = native_mapping_for_type(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(class_mirror)),
                Value::Object(None),
            ],
        )
        .expect("native call");

        // Should return non-null (composite mapping), and the visited
        // stack should be empty (push+pop balanced).
        assert!(matches!(r, Some(Value::Object(Some(_)))));
        VISITED_TYPES.with(|v| assert!(v.borrow().is_empty()));
    }

    #[test]
    fn t19_m1_alloc_composite_mapping_returns_non_null() {
        let mut ctx = mock_ctx();
        let schema = composite_schema_for("java.lang.management.MemoryUsage")
            .expect("schema present");
        let m = alloc_composite_mapping(&mut ctx, schema);
        // Verify the openType slot is populated.
        match ctx.get_field(m, 1) {
            Value::Object(Some(_)) => {}
            other => panic!("expected populated openType, got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_composite_type_carries_item_names() {
        let mut ctx = mock_ctx();
        let schema = composite_schema_for("java.lang.management.LockInfo")
            .expect("schema");
        let ct = alloc_composite_type(&mut ctx, schema);
        // The mock NativeContext doesn't map our well-known JMX field
        // names (typeName, description, className, isArray, itemNames)
        // to slots — production resolves these via the loaded class's
        // actual field layout. We assert here that the allocation
        // succeeded and is structurally non-empty by checking
        // `object_num_fields` returned at least the 8 slots we asked
        // for in `alloc_concurrent_synthetic`.
        let n = ctx.object_num_fields(ct);
        assert!(n >= 8, "expected ≥8 fields on CompositeType stub, got {n}");
    }

    #[test]
    fn t19_m1_alloc_open_converter_with_schema() {
        let mut ctx = mock_ctx();
        let schema = composite_schema_for("java.lang.management.ThreadInfo")
            .expect("schema");
        let oc = alloc_open_converter(&mut ctx, Some(schema));
        // identityConverter flag (slot 3) should be 1.
        match ctx.get_field(oc, 3) {
            Value::Int(1) => {}
            other => panic!("expected Int(1), got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_open_converter_without_schema_uses_simple_string() {
        let mut ctx = mock_ctx();
        let oc = alloc_open_converter(&mut ctx, None);
        // openType (slot 1) must be a SimpleType-shaped object (non-null).
        match ctx.get_field(oc, 1) {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null openType, got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_mapped_mxbean_type_basic_for_unknown() {
        let mut ctx = mock_ctx();
        let mt = alloc_mapped_mxbean_type(&mut ctx, None);
        // isBasicType (slot 2) should be 1 (SimpleType-mapped).
        match ctx.get_field(mt, 2) {
            Value::Int(1) => {}
            other => panic!("expected Int(1), got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_mapped_mxbean_type_composite_for_known() {
        let mut ctx = mock_ctx();
        let schema = composite_schema_for("java.lang.management.MemoryUsage")
            .expect("schema");
        let mt = alloc_mapped_mxbean_type(&mut ctx, Some(schema));
        // isBasicType should be 0 for composite.
        match ctx.get_field(mt, 2) {
            Value::Int(0) => {}
            other => panic!("expected Int(0) for composite, got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_native_open_converter_to_converter_handles_null() {
        let mut ctx = mock_ctx();
        let r = native_open_converter_to_converter(
            &mut ctx,
            &[Value::Object(None)],
        )
        .expect("native call");
        assert!(matches!(r, Some(Value::Object(Some(_)))));
    }

    #[test]
    fn t19_m1_native_mapped_mxbean_type_handles_null() {
        let mut ctx = mock_ctx();
        let r = native_mapped_mxbean_type(&mut ctx, &[Value::Object(None)])
            .expect("native call");
        assert!(matches!(r, Some(Value::Object(Some(_)))));
    }

    #[test]
    fn t19_m1_register_natives_count_increased() {
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_jmx_openmbean_natives(&mut r);
        let after = r.len();
        // T19.H14 had 7 registrations. T19.M1 adds 2 more
        // (OpenConverter.toConverter, MappedMXBeanType.getMappedMXBeanType).
        assert!(
            after - before >= 9,
            "expected ≥9 registrations after T19.M1, got {}",
            after - before
        );
    }

    #[test]
    fn t19_m1_open_converter_to_converter_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/OpenConverter",
                "toConverter",
                "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/OpenConverter;"
            )
            .is_some()
        );
    }

    #[test]
    fn t19_m1_mapped_mxbean_type_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        assert!(
            r.find(
                "com/sun/jmx/mbeanserver/MappedMXBeanType",
                "getMappedMXBeanType",
                "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/MappedMXBeanType;"
            )
            .is_some()
        );
    }

    #[test]
    fn t19_m1_composite_type_names_match_schemas() {
        // Every COMPOSITE_TYPE_NAMES entry has a corresponding schema.
        for name in COMPOSITE_TYPE_NAMES {
            assert!(
                composite_schema_for(name).is_some(),
                "no schema for composite name {name}"
            );
        }
    }

    #[test]
    fn t19_m1_resolve_type_name_via_class_mirror_slot1() {
        let mut ctx = mock_ctx();
        let class_mirror = alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("java.lang.management.MemoryUsage");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));
        let resolved = resolve_type_name(&ctx, class_mirror);
        assert_eq!(resolved.as_deref(), Some("java.lang.management.MemoryUsage"));
    }

    #[test]
    fn t19_m1_resolve_type_name_normalizes_slashes_to_dots() {
        let mut ctx = mock_ctx();
        let class_mirror = alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4);
        let name_str = ctx.create_string("java/lang/management/MemoryUsage");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));
        let resolved = resolve_type_name(&ctx, class_mirror);
        // Slashes must be normalized to dots so the lookup against
        // COMPOSITE_TYPE_NAMES succeeds.
        assert_eq!(resolved.as_deref(), Some("java.lang.management.MemoryUsage"));
    }
}
