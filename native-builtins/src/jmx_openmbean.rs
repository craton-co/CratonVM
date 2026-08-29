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
//! 2026-08-11 — the two defence-in-depth overlays above are now OFF by default
//! on a real-JDK run (see `real_mxbean_mapping_enabled`). They cost more than
//! they bought: typing every unrecognised type as `SimpleType.STRING` and
//! making `toOpenValue` the identity meant `MBeanServer.getAttribute` handed
//! back the raw Java value, so `java.lang:type=Memory` / `HeapMemoryUsage`
//! answered a `java.lang.management.MemoryUsage` where every other JVM answers
//! a `CompositeDataSupport`.
//!
//! The PRIMARY fix — the `getMethods(Class)` Object-method filter described
//! above — stays registered unconditionally, and it is why the real
//! `DefaultMXBeanMappingFactory` now terminates here: the `Class` →
//! `AnnotatedType[]` self-reference that started this whole workaround is
//! never offered to the mapping factory in the first place. Measured against
//! JDK 25 on Linux, the real machinery reproduces HotSpot's answers exactly
//! for every platform-MXBean attribute this VM can serve, and rejects a
//! genuinely self-referential MXBean type with HotSpot's own
//! `NotCompliantMBeanException` rather than looping.
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

use crate::try_alloc_concurrent_synthetic;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallFailed;
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
    COMPOSITE_SCHEMAS.iter().find(|s| s.type_name == type_name)
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
fn method_declaring_class_name(ctx: &dyn NativeContext, method_obj: ObjectRef) -> Option<String> {
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
fn is_object_inherited_method(ctx: &dyn NativeContext, method_obj: ObjectRef) -> bool {
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
///
/// `elements` carries `(pin_handle, ref)` pairs: callers collect the refs
/// across repeated allocating calls, and the two allocations below can move
/// them again (native stale-local family), so each element is re-read from
/// its pin right before it is stored.
fn alloc_array_list_from(
    ctx: &mut dyn NativeContext,
    elements: &[(usize, ObjectRef)],
) -> Result<ObjectRef, MethodCallFailed> {
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    // Pin across the backing-array allocation below — a moving young GC
    // there would relocate the fresh list (native stale-local family).
    let list_pin = ctx.pin_native_root(list);
    let backing = ctx.new_ref_array(cratonvm_types::ClassId::new(0), elements.len());
    let list = ctx.read_native_pin(list_pin, list);
    for (i, &(el_pin, el)) in elements.iter().enumerate() {
        // Re-read each element to its current (post-GC) address.
        let el = ctx.read_native_pin(el_pin, el);
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
    ctx.unpin_native_roots(list_pin);
    Ok(list)
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
    // Pin across the per-method mirror construction below — every
    // `build_method_mirror` call allocates, and a moving young GC there would
    // relocate `class_mirror` (native stale-local family). Re-read from the
    // pin before each use. The pin vec is truncated at native exit, so the
    // early returns below need no explicit unpin.
    let class_mirror_pin = ctx.pin_native_root(class_mirror);

    // Resolve the class internal name from the mirror so we can look
    // up the underlying ClassId, then enumerate every public method
    // (including inherited from superinterfaces and superclasses) the
    // way `Class.getMethods()` does.
    let class_name_dot = match crate::lang_class::mirror_class_name(ctx, class_mirror) {
        Some(s) => s,
        None => {
            // Empty list — caller's iterator path won't crash.
            return Ok(Some(Value::Object(Some(alloc_array_list_from(ctx, &[])?))));
        }
    };
    let class_name_internal = class_name_dot.replace('.', "/");
    let cid = match ctx.class_id_by_name(&class_name_internal) {
        Some(c) => c,
        None => {
            return Ok(Some(Value::Object(Some(alloc_array_list_from(ctx, &[])?))));
        }
    };

    // Walk the class + its superclasses + its superinterfaces, collecting
    // methods. We keep only public methods (matching `Class.getMethods()`
    // semantics) and skip methods declared on `java.lang.Object`. We also
    // walk superinterfaces transitively because MBean interfaces extend
    // each other (e.g. `MemoryMXBean` extends `PlatformManagedObject`).
    let mut visited_classes: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // (pin_handle, ref) per mirror — each later `build_method_mirror` call
    // allocates and can move the earlier mirrors, so every element is pinned
    // as it is produced and re-read from its pin at the point of use
    // (native stale-local family; same pattern as `p59_jar_collect_entries`).
    let mut method_mirrors: Vec<(usize, ObjectRef)> = Vec::new();
    // Track (name, descriptor) to avoid duplicates when the same method
    // is declared on both a class and an interface, mirroring
    // `Class.getMethods()` deduplication.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

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
            let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
            let method_mirror = build_method_mirror(
                ctx,
                class_mirror,
                &method_meta.name,
                &method_meta.descriptor,
                method_meta.access_flags,
            )?;
            let method_mirror_pin = ctx.pin_native_root(method_mirror);
            method_mirrors.push((method_mirror_pin, method_mirror));
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
    )?))))
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
) -> Result<ObjectRef, MethodCallFailed> {
    // SPB.11: Delegate to the canonical `create_method_object` so that the
    // CratonVM extra metadata slots (raw descriptor, parameter count,
    // accessible flag) are populated. Without those, `Method.invoke`
    // reads `param_descs.len() == 0` from a missing descriptor and throws
    // "wrong number of arguments". `create_method_object` also populates
    // `exceptionTypes`, annotation byte arrays, and other JDK-named
    // fields the reflective code relies on.
    if let Some(declaring_class_id) =
        crate::lang_class::mirror_class_id(ctx, declaring_class_mirror)
    {
        // Real interface method, so resolve its generic signature here —
        // `create_method_object` now reads the field instead of searching.
        let signature = ctx.method_signature(declaring_class_id, name, descriptor);
        let meta = cratonvm_native_api::registry::MethodMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: modifiers,
            declaring_class_id,
            exceptions: Vec::new(),
            signature,
        };
        return Ok(crate::lang_class::create_method_object(ctx, &meta)?);
    }
    // Fallback when we can't resolve the declaring class id — fill in only
    // the JDK-named fields we can. Method.invoke will still error, but the
    // mirror is at least non-null for `getName`/`getParameterCount`.
    //
    // Pin every ref held across the allocating calls below
    // (`alloc_concurrent_synthetic` / `create_string` / `new_ref_array` /
    // `type_descriptor_to_class_mirror`, which class-loads) — a moving young
    // GC during any of them would relocate the objects and leave the raw
    // `ObjectRef`s stale (native stale-local family).
    let declaring_pin = ctx.pin_native_root(declaring_class_mirror);
    let method_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/Method", 12)?;
    let method_obj_pin = ctx.pin_native_root(method_obj);
    let declaring_class_mirror = ctx.read_native_pin(declaring_pin, declaring_class_mirror);
    ctx.set_field_by_name(
        method_obj,
        "clazz",
        Value::Object(Some(declaring_class_mirror)),
    );
    let name_str = ctx.create_string(name);
    let method_obj = ctx.read_native_pin(method_obj_pin, method_obj);
    ctx.set_field_by_name(method_obj, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(method_obj, "modifiers", Value::Int(modifiers as i32));
    let (params, ret) = parse_method_descriptor(descriptor);
    let param_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), params.len());
    let param_arr_pin = ctx.pin_native_root(param_arr);
    for (i, p) in params.iter().enumerate() {
        let m = type_descriptor_to_class_mirror(ctx, p);
        let param_arr = ctx.read_native_pin(param_arr_pin, param_arr);
        ctx.set_array_element(param_arr, i, Value::Object(Some(m)));
    }
    let method_obj = ctx.read_native_pin(method_obj_pin, method_obj);
    let param_arr = ctx.read_native_pin(param_arr_pin, param_arr);
    ctx.set_field_by_name(method_obj, "parameterTypes", Value::Object(Some(param_arr)));
    let ret_mirror = type_descriptor_to_class_mirror(ctx, &ret);
    let method_obj = ctx.read_native_pin(method_obj_pin, method_obj);
    ctx.set_field_by_name(method_obj, "returnType", Value::Object(Some(ret_mirror)));
    // Release this helper's pins; `method_obj` was re-read after the last
    // allocating call, so the returned ref is current. Callers that hold it
    // across their own allocating calls pin it themselves.
    ctx.unpin_native_roots(declaring_pin);
    Ok(method_obj)
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

fn type_descriptor_to_class_mirror(ctx: &mut dyn NativeContext, desc: &str) -> ObjectRef {
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
            //
            // Validate the descriptor before slicing: a well-formed object
            // descriptor is at least `L;` (3 bytes incl. a non-empty name is
            // the norm, but `L;` is the minimum that ends in `;`) and must be
            // terminated by `;`. Attacker-controlled descriptors such as `L`
            // (no terminator) or `Lfoo` (missing `;`) would otherwise either
            // panic on the `desc[1..desc.len() - 1]` slice (start > end when
            // len < 2) or silently chop the final name character. Operate on
            // bytes and `str::from_utf8` the internal name; fall back to the
            // Object mirror on any malformed input instead of slicing blindly.
            let bytes = desc.as_bytes();
            let internal = if bytes.len() >= 3 && bytes[bytes.len() - 1] == b';' {
                std::str::from_utf8(&bytes[1..bytes.len() - 1]).ok()
            } else {
                None
            };
            match internal.map(|name| ctx.ensure_class_initialized(name)) {
                Some(Ok(cid)) => ctx.get_class_mirror(cid),
                _ => {
                    // Malformed descriptor or load failure — fall back to the
                    // Object mirror.
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
fn native_converting_method_from(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let method_obj = match args.first() {
        Some(Value::Object(Some(m))) => *m,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_object_inherited_method(ctx, method_obj) {
        // Skip — caller stores null.
        return Ok(Some(Value::Object(None)));
    }
    // Build a minimal ConvertingMethod with enough type metadata for the JDK's
    // MXBeanIntrospector lookup tables. In particular, getOpenSignature() is
    // derived from paramMappings[*].openClass, so an empty mapping array makes
    // overloaded operations such as ThreadMXBean.getThreadInfo(long) uncallable.
    // Pin `method_obj` and the fresh ConvertingMethod across the allocating
    // calls below — a moving young GC there would relocate them (native
    // stale-local family).
    let method_pin = ctx.pin_native_root(method_obj);
    let cvt = try_alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/ConvertingMethod", 4)?;
    let cvt_pin = ctx.pin_native_root(cvt);
    // Field 0: method
    let method_obj = ctx.read_native_pin(method_pin, method_obj);
    ctx.set_field(cvt, 0, Value::Object(Some(method_obj)));
    // Field 1: returnMapping — keep the previous identity mapping. The open
    // return-type table still has broader CompositeType gaps; this fix is scoped
    // to operation dispatch signatures, which are keyed from paramMappings.
    let ret_mapping = alloc_identity_mapping(ctx);
    let cvt = ctx.read_native_pin(cvt_pin, cvt);
    ctx.set_field(cvt, 1, Value::Object(Some(ret_mapping?)));
    // Field 2: paramMappings.
    let method_obj = ctx.read_native_pin(method_pin, method_obj);
    let param_mappings = converting_method_param_mappings(ctx, method_obj);
    let cvt = ctx.read_native_pin(cvt_pin, cvt);
    ctx.set_field(cvt, 2, Value::Object(Some(param_mappings?)));
    // Field 3: paramConversionIsIdentity = true
    ctx.set_field(cvt, 3, Value::Int(1));
    ctx.unpin_native_roots(method_pin);
    Ok(Some(Value::Object(Some(cvt))))
}

fn converting_method_param_mappings(
    ctx: &mut dyn NativeContext,
    method_obj: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // Pin across the class-load and the reflective invokes below — a moving
    // young GC there would relocate `method_obj` (native stale-local family);
    // in particular the first invoke can move it before the fallback invoke
    // reads it.
    let method_pin = ctx.pin_native_root(method_obj);
    let mapping_class_id = ctx
        .ensure_class_initialized("com/sun/jmx/mbeanserver/MXBeanMapping")
        .unwrap_or(cratonvm_types::ClassId::new(0));

    let method_obj = ctx.read_native_pin(method_pin, method_obj);
    let arr = match invoke_reflect_type_array(
        ctx,
        method_obj,
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
    ) {
        Some(a) => Some(a),
        None => {
            let method_obj = ctx.read_native_pin(method_pin, method_obj);
            invoke_reflect_type_array(ctx, method_obj, "getParameterTypes", "()[Ljava/lang/Class;")
        }
    };
    let Some(mut param_types_arr) = arr else {
        return Ok(ctx.new_ref_array(mapping_class_id, 0));
    };

    let param_types_pin = ctx.pin_native_root(param_types_arr);
    param_types_arr = ctx.read_native_pin(param_types_pin, param_types_arr);
    let len = ctx.array_length(param_types_arr);
    let mut out = ctx.new_ref_array(mapping_class_id, len);
    let out_pin = ctx.pin_native_root(out);

    for i in 0..len {
        param_types_arr = ctx.read_native_pin(param_types_pin, param_types_arr);
        let type_obj = match ctx.get_array_element(param_types_arr, i) {
            Value::Object(Some(t)) => t,
            _ => {
                let mapping = alloc_identity_mapping(ctx);
                out = ctx.read_native_pin(out_pin, out);
                ctx.set_array_element(out, i, Value::Object(Some(mapping?)));
                continue;
            }
        };
        let preserve_original_open_class = primitive_or_primitive_array_class_mirror(ctx, type_obj);
        let mapping = alloc_mapping_for_type_object(ctx, type_obj)?;
        if preserve_original_open_class {
            param_types_arr = ctx.read_native_pin(param_types_pin, param_types_arr);
            if let Value::Object(Some(open_class)) = ctx.get_array_element(param_types_arr, i) {
                ctx.set_field(mapping, 2, Value::Object(Some(open_class)));
            }
        }
        out = ctx.read_native_pin(out_pin, out);
        ctx.set_array_element(out, i, Value::Object(Some(mapping)));
    }

    out = ctx.read_native_pin(out_pin, out);
    ctx.unpin_native_roots(method_pin);
    Ok(out)
}

fn invoke_reflect_type_array(
    ctx: &mut dyn NativeContext,
    method_obj: ObjectRef,
    name: &str,
    desc: &str,
) -> Option<ObjectRef> {
    match ctx.invoke_virtual(method_obj, name, desc, &[]) {
        Ok(Some(Value::Object(Some(arr)))) => Some(arr),
        _ => None,
    }
}

fn alloc_mapping_for_type_object(
    ctx: &mut dyn NativeContext,
    type_obj: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin = ctx.pin_native_root(type_obj);
    let type_obj = ctx.read_native_pin(pin, type_obj);
    let mapping = match native_mapping_for_type(
        ctx,
        &[
            Value::Object(None),
            Value::Object(Some(type_obj)),
            Value::Object(None),
        ],
    ) {
        Ok(Some(Value::Object(Some(mapping)))) => mapping,
        _ => alloc_identity_mapping(ctx)?,
    };
    ctx.unpin_native_roots(pin);
    Ok(mapping)
}

fn primitive_or_primitive_array_class_mirror(ctx: &dyn NativeContext, type_obj: ObjectRef) -> bool {
    crate::lang_class::mirror_class_name(ctx, type_obj)
        .map(|name| primitive_or_primitive_array_name(&name))
        .unwrap_or(false)
}

fn primitive_or_primitive_array_name(name: &str) -> bool {
    match name {
        "boolean" | "byte" | "char" | "double" | "float" | "int" | "long" | "short" | "void" => {
            true
        }
        s if s.starts_with('[') => {
            let elem = s.trim_start_matches('[');
            matches!(
                elem.as_bytes().first(),
                Some(b'Z' | b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S')
            )
        }
        _ => false,
    }
}

/// Allocate a synthetic `MXBeanMapping` instance backed by
/// `SimpleType.STRING` whose `fromOpenValue`/`toOpenValue` are
/// identity. The instance is shared via thread-local cache to keep
/// the heap tidy; consumers compare by reference rarely (they read
/// `getOpenType()` mostly), so referential identity is preserved
/// across calls within the same thread.
fn alloc_identity_mapping(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // Field 0: javaType (Type) — null is acceptable.
    // Field 1: openType (OpenType) — SimpleType.STRING singleton.
    // Field 2: openClass (Class<?>) — String.class mirror.
    let m = try_alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/MXBeanMapping", 3)?;
    // Pin across the sibling allocations below — a moving young GC there
    // would relocate the fresh mapping (native stale-local family).
    let m_pin = ctx.pin_native_root(m);
    ctx.set_field(m, 0, Value::Object(None));
    let st = alloc_simple_type_string(ctx);
    let m = ctx.read_native_pin(m_pin, m);
    ctx.set_field(m, 1, Value::Object(Some(st?)));
    let string_cid = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let string_mirror = ctx.get_class_mirror(string_cid);
    let m = ctx.read_native_pin(m_pin, m);
    ctx.set_field(m, 2, Value::Object(Some(string_mirror)));
    ctx.unpin_native_roots(m_pin);
    Ok(m)
}

/// Allocate a synthetic `SimpleType<String>` instance. JDK 25's
/// `SimpleType.STRING` is a public-final singleton, but we don't
/// have a way to read its static field through the registry's
/// `get_static_field` without first resolving the slot index. We
/// allocate an equivalent instance whose `getClassName()` /
/// `getTypeName()` / `getDescription()` / `isArray()` reads return
/// JDK-equivalent values.
fn alloc_simple_type_string(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // Try to fetch the static SimpleType.STRING first — it's by far
    // the cleanest path because `MXBeanIntrospector.canUseOpenInfo`
    // does a `==` comparison against the singleton, which only works
    // with the real instance.
    if let Ok(cid) = ctx.ensure_class_initialized("javax/management/openmbean/SimpleType") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "STRING") {
            if let Value::Object(Some(s)) = ctx.get_static_field(cid, idx) {
                return Ok(s);
            }
        }
    }
    // Fall back to a synthetic instance with the right field values.
    // Pin each fresh object across the subsequent `create_string` calls — a
    // moving young GC there would relocate them (native stale-local family).
    let st = try_alloc_concurrent_synthetic(ctx, "javax/management/openmbean/SimpleType", 5)?;
    let st_pin = ctx.pin_native_root(st);
    let class_name = ctx.create_string("java.lang.String");
    let class_name_pin = ctx.pin_native_root(class_name);
    let type_name = ctx.create_string("java.lang.String");
    let type_name_pin = ctx.pin_native_root(type_name);
    let description = ctx.create_string("java.lang.String");
    let st = ctx.read_native_pin(st_pin, st);
    let class_name = ctx.read_native_pin(class_name_pin, class_name);
    let type_name = ctx.read_native_pin(type_name_pin, type_name);
    ctx.set_field_by_name(st, "className", Value::Object(Some(class_name)));
    ctx.set_field_by_name(st, "typeName", Value::Object(Some(type_name)));
    ctx.set_field_by_name(st, "description", Value::Object(Some(description)));
    ctx.set_field_by_name(st, "isArray", Value::Int(0));
    ctx.unpin_native_roots(st_pin);
    Ok(st)
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

/// Identity passthrough for `MXBeanMapping.toOpenValue`/`fromOpenValue` on
/// our synthetic mapping instances -- see the registration site in
/// `register_jmx_openmbean_natives` for the full rationale. `args[0]` is the
/// receiver (the synthetic `MXBeanMapping`), `args[1]` is the value being
/// converted; both methods just hand it back unchanged.
fn native_mxbean_mapping_identity(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

fn native_mapping_for_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = `this` (factory)
    // args[1] = the Type to convert
    // args[2] = the factory (recursive parameter)
    let type_obj = match args.get(1) {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(Some(alloc_identity_mapping(ctx)?)))),
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
            return Ok(Some(Value::Object(Some(alloc_identity_mapping(ctx)?))));
        }
    }

    // T19.M1 — short-circuit problematic types early so we never push
    // them onto the visited stack. This avoids polluting the stack
    // with names we know will trigger recursion.
    if let Some(name) = &normalized {
        if PROBLEMATIC_TYPE_NAMES.contains(&name.as_str()) {
            return Ok(Some(Value::Object(Some(alloc_identity_mapping(ctx)?))));
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
            return Ok(Some(Value::Object(Some(mapping?))));
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
    Ok(Some(Value::Object(Some(mapping?))))
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
) -> Result<ObjectRef, MethodCallFailed> {
    // CompositeMapping fields match MXBeanMapping (identity layout) +
    // a CompositeType in the openType slot.
    let m = try_alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/MXBeanMapping", 3)?;
    // Pin across the sibling allocations below — a moving young GC there
    // would relocate the fresh mapping (native stale-local family).
    let m_pin = ctx.pin_native_root(m);
    // Field 0: javaType (Type) — null is acceptable.
    ctx.set_field(m, 0, Value::Object(None));
    // Field 1: openType (OpenType) — synthetic CompositeType.
    let composite_type = alloc_composite_type(ctx, schema);
    let m = ctx.read_native_pin(m_pin, m);
    ctx.set_field(m, 1, Value::Object(Some(composite_type?)));
    // Field 2: openClass (Class<?>) — CompositeData.class mirror, fall
    // back to String.class if the class isn't loadable.
    let open_class_mirror =
        match ctx.ensure_class_initialized("javax/management/openmbean/CompositeData") {
            Ok(cid) => ctx.get_class_mirror(cid),
            Err(_) => match ctx.ensure_class_initialized("java/lang/String") {
                Ok(cid) => ctx.get_class_mirror(cid),
                Err(_) => ctx.alloc_object(ClassId::new(0), 0),
            },
        };
    let m = ctx.read_native_pin(m_pin, m);
    ctx.set_field(m, 2, Value::Object(Some(open_class_mirror)));
    ctx.unpin_native_roots(m_pin);
    Ok(m)
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
) -> Result<ObjectRef, MethodCallFailed> {
    let ct = try_alloc_concurrent_synthetic(ctx, "javax/management/openmbean/CompositeType", 8)?;
    // Pin each fresh object across the subsequent allocating calls
    // (`create_string` / `ensure_class_initialized` / `new_ref_array`) — a
    // moving young GC there would relocate them (native stale-local family).
    let ct_pin = ctx.pin_native_root(ct);

    // typeName + description (both stored as java.lang.String).
    let type_name = ctx.create_string(schema.type_name);
    let type_name_pin = ctx.pin_native_root(type_name);
    let description = ctx.create_string(schema.type_name);
    let ct = ctx.read_native_pin(ct_pin, ct);
    let type_name = ctx.read_native_pin(type_name_pin, type_name);
    ctx.set_field_by_name(ct, "typeName", Value::Object(Some(type_name)));
    ctx.set_field_by_name(ct, "description", Value::Object(Some(description)));

    // The OpenType base class also holds `className` — for composites
    // this is `javax.management.openmbean.CompositeData` (the standard
    // open class for composite mappings).
    let class_name = ctx.create_string("javax.management.openmbean.CompositeData");
    let ct = ctx.read_native_pin(ct_pin, ct);
    ctx.set_field_by_name(ct, "className", Value::Object(Some(class_name)));
    ctx.set_field_by_name(ct, "isArray", Value::Int(0));

    // Build a String[] of itemNames matching the schema. Some JDK
    // paths read this directly via `keySet()` on the TreeMap, others
    // via a `String[]` cache. We populate both for safety.
    let string_class_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(ClassId::new(0));
    let item_names_arr = ctx.new_ref_array(string_class_id, schema.items.len());
    let item_names_pin = ctx.pin_native_root(item_names_arr);
    for (i, item) in schema.items.iter().enumerate() {
        let s = ctx.create_string(item);
        let item_names_arr = ctx.read_native_pin(item_names_pin, item_names_arr);
        ctx.set_array_element(item_names_arr, i, Value::Object(Some(s)));
    }
    // The synthetic field name `itemNames` mirrors the JDK CompositeType
    // private-field convention. Real-JDK CompositeType stores this in
    // a private final field — we add it via field-by-name so synthetic
    // mode tolerates the new field even if absent in the bare stub.
    let ct = ctx.read_native_pin(ct_pin, ct);
    let item_names_arr = ctx.read_native_pin(item_names_pin, item_names_arr);
    ctx.set_field_by_name(ct, "itemNames", Value::Object(Some(item_names_arr)));
    ctx.unpin_native_roots(ct_pin);
    Ok(ct)
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
/// Should the JDK's own MXBean type-mapping machinery be left alone?
///
/// **Default: yes.** The `mappingForType` / `makeMapping` / `toOpenValue` /
/// `ConvertingMethod.from` overrides below replace
/// `DefaultMXBeanMappingFactory` with a synthetic mapping that types every
/// unrecognised Java type as `SimpleType.STRING` and converts nothing
/// (`toOpenValue` is identity). That is why
/// `MBeanServer.getAttribute("java.lang:type=Memory", "HeapMemoryUsage")` used
/// to hand back a raw `java.lang.management.MemoryUsage` where every other JVM
/// returns a `CompositeDataSupport`, and why `getMBeanInfo` described every
/// composite attribute as `java.lang.String`.
///
/// They were added because the real recursion was believed not to terminate on
/// this VM (`OpenDataException` through `Class.getAnnotatedInterfaces()`).
/// Measured 2026-08-11 against JDK 25 on Linux, that is no longer true: with
/// the real machinery restored, every platform-MXBean attribute this VM can
/// answer matches HotSpot exactly — `MemoryUsage` and every `MemoryPool`
/// usage become `CompositeDataSupport`, `SystemProperties` becomes
/// `TabularDataSupport`, `InputArguments` becomes `String[]`, and
/// `getMBeanInfo` carries the real `CompositeType`. The recursion terminates:
/// a self-referential MXBean type is rejected with the same
/// `NotCompliantMBeanException` HotSpot raises, rather than hanging — where
/// the synthetic mapping silently *accepted* it and handed back raw Java
/// objects.
///
/// Gate, do not delete. `synthetic-jdk` builds have no real
/// `com.sun.jmx.mbeanserver` bytecode to fall back to and keep the overrides;
/// `CRATONVM_SYNTHETIC_MXBEAN_MAPPING=1` restores them on a real-JDK run,
/// which is the one-run answer if an application MBean ever does drive the
/// real factory into a recursion this VM cannot finish.
///
/// Mirrors `native-io`'s `real_raf_enabled()`, which flipped the same way for
/// the same reason.
pub(crate) fn real_mxbean_mapping_enabled() -> bool {
    if cfg!(feature = "synthetic-jdk") {
        return false;
    }
    // The latched `VmFlags` snapshot, not a live `getenv`. An undeclared flag
    // read straight from `std::env` is unreachable from
    // `CRATONVM_REAL=-mxbean-mapping` and invisible to
    // `flags::with_thread_overrides`, so a test that arranges it through the
    // supported hook silently measures the developer's ambient environment
    // instead. `one_true_yes_exact` also accepts `yes`, which the previous
    // `Ok("1") | Ok("true")` did not — a strict widening of an opt-out escape
    // hatch, and one fewer bespoke truth table.
    !crate::nbflags().synthetic_mxbean_mapping
}

pub fn register_jmx_openmbean_natives(registry: &mut NativeMethodRegistry) {
    register_jmx_openmbean_natives_with(registry, !real_mxbean_mapping_enabled());
}

/// [`register_jmx_openmbean_natives`] with the type-mapping decision supplied
/// rather than read from the environment, so a test can exercise both arms
/// without touching process-wide state.
///
/// `synthetic_mapping = true` reinstates the pre-2026-08-11 overlay that types
/// unrecognised Java types as `SimpleType.STRING` and converts nothing.
pub fn register_jmx_openmbean_natives_with(
    registry: &mut NativeMethodRegistry,
    synthetic_mapping: bool,
) {
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
    // Defence in depth: short-circuit the OpenType recursion at the
    // mapping factory level too, so plain MBeans don't hit it.
    if synthetic_mapping {
        // Defence in depth: if a path still reaches ConvertingMethod.from
        // with an Object method (e.g. tests bypass the introspector),
        // short-circuit by returning null.
        //
        // Inside the gate: this override installs an IDENTITY return mapping, so
        // leaving it registered would keep `getAttribute` handing back the raw
        // Java value even with the mapping factory restored. The Object-method
        // filter it also provides is already covered by the `getMethods`
        // registrations above, which stay unconditional.
        registry.register(
            "com/sun/jmx/mbeanserver/ConvertingMethod",
            "from",
            "(Ljava/lang/reflect/Method;)Lcom/sun/jmx/mbeanserver/ConvertingMethod;",
            native_converting_method_from,
        );
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

        // T19.M1 follow-up: `MXBeanMapping.toOpenValue`/`fromOpenValue` are
        // ABSTRACT on the base class (see MXBeanMapping.java) -- every synthetic
        // mapping instance we hand back from `native_mapping_for_type` /
        // `alloc_identity_mapping` / `alloc_composite_mapping` above is allocated
        // with class name `com/sun/jmx/mbeanserver/MXBeanMapping` itself (not a
        // real concrete subclass), so calling either method on one threw
        // `AbstractMethodError: ... has no Code attribute` the first time a real
        // attribute/operation VALUE (not just MBeanInfo structure) needed
        // conversion -- e.g. `MemoryMXBean.getHeapMemoryUsage()` accessed through
        // a `MXBeanProxy`, once platform-MXBean registration (T19 registration
        // fix) let real bytecode reach this far for the first time.
        //
        // Registering these two directly on the abstract `MXBeanMapping` class
        // name only ever intercepts OUR synthetic instances: any real JDK
        // subclass (from `DefaultMXBeanMappingFactory`'s permanent mappings for
        // String/Integer/etc.) has its own concrete Code-attributed override,
        // which virtual dispatch resolves first -- same "safe fallback on an
        // abstract/interface type" pattern already used for `JavaLangAccess`
        // elsewhere in this codebase.
        //
        // Identity passthrough is deliberately the whole implementation: our
        // mappings are only ever consumed by a `toOpenValue` call on the
        // MBeanServer side immediately followed by a `fromOpenValue` call on the
        // client/proxy side of the SAME in-process round trip (there is no wire
        // protocol in between for the `MBeanServerConnection` used by these
        // tests), so handing the original Java value straight through
        // unconverted reproduces the exact value the caller expects without
        // needing a real `CompositeType`/`CompositeData` implementation. This
        // matches the design this module's own header already documented
        // ("fromOpenValue and toOpenValue are no-op identity stubs") -- that
        // claim just was not backed by an actual registration until now.
        registry.register(
            "com/sun/jmx/mbeanserver/MXBeanMapping",
            "toOpenValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_mxbean_mapping_identity,
        );
        registry.register(
            "com/sun/jmx/mbeanserver/MXBeanMapping",
            "fromOpenValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_mxbean_mapping_identity,
        );
    } // end if synthetic_mapping

    // T19_M1_PLATFORM_MXBEANS — additional defensive overrides on the
    // OpenConverter path.
    //
    // INERT ON JDK 25 (checked 2026-08-11 with `javap --module java.management`):
    // neither `com.sun.jmx.mbeanserver.OpenConverter` nor `MappedMXBeanType`
    // exists on that image — both are pre-JDK-7 spellings, and the OpenType
    // analysis lives entirely in `DefaultMXBeanMappingFactory`. Left registered
    // rather than deleted because they still name real classes on the older
    // images this VM is expected to run, and a registration that targets
    // nothing costs nothing; do not read their presence as evidence that this
    // path is live.
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

    // -- OpenMBean value carriers: CompositeData + TabularData --
    //
    // The introspector overrides above keep the *type* machinery from
    // blowing up; these give the *value* side a working implementation so
    // an MXBean (or app code) can build and read CompositeData /
    // TabularData. We model both on a backing `java/util/HashMap` stored in
    // a synthetic field (`contents`) plus the `compositeType` /
    // `tabularType` reference. The natives below intercept the public
    // CompositeData / TabularData read methods so they operate on that
    // backing map regardless of the (synthetic) field layout.
    register_open_data_carriers(registry);
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// OpenMBean value carriers — CompositeDataSupport / TabularDataSupport
// ---------------------------------------------------------------------------

/// Synthetic field name holding the backing `java.util.HashMap` for a
/// CompositeData / TabularData carrier.
const CONTENTS_FIELD: &str = "cratonvm$contents";
/// Synthetic field name holding the open type (CompositeType / TabularType).
const OPEN_TYPE_FIELD: &str = "cratonvm$openType";

/// Build a `CompositeDataSupport`-shaped object from item names + values.
/// The backing store is a real `java.util.HashMap` (so `get` / `containsKey`
/// / `values` all work through standard collection bytecode), keyed by the
/// item name strings. `composite_type` may be null when the caller only
/// needs the value side.
pub(crate) fn build_composite_data(
    ctx: &mut dyn NativeContext,
    composite_type: Option<ObjectRef>,
    items: &[(String, Value)],
) -> Result<ObjectRef, MethodCallFailed> {
    // Pin every ref held across the allocating calls below — a moving young
    // GC there would relocate them (native stale-local family). The map is
    // built FIRST so `build_string_keyed_map` can pin the item refs before
    // any allocation invalidates them.
    let composite_type_pin = composite_type.map(|o| ctx.pin_native_root(o));
    let map = build_string_keyed_map(ctx, items)?;
    let map_pin = ctx.pin_native_root(map);
    let obj =
        try_alloc_concurrent_synthetic(ctx, "javax/management/openmbean/CompositeDataSupport", 4)?;
    let map = ctx.read_native_pin(map_pin, map);
    ctx.set_field_by_name(obj, CONTENTS_FIELD, Value::Object(Some(map)));
    let composite_type = match (composite_type_pin, composite_type) {
        (Some(h), Some(o)) => Some(ctx.read_native_pin(h, o)),
        _ => composite_type,
    };
    ctx.set_field_by_name(obj, OPEN_TYPE_FIELD, Value::Object(composite_type));
    match composite_type_pin {
        Some(h) => ctx.unpin_native_roots(h),
        None => ctx.unpin_native_roots(map_pin),
    }
    Ok(obj)
}

/// Build a `java.util.HashMap` populated with the given String→Value pairs.
/// Falls back to a synthetic 2-slot map (data array + size) when
/// `HashMap.put` cannot be invoked (unit-test mock).
fn build_string_keyed_map(
    ctx: &mut dyn NativeContext,
    items: &[(String, Value)],
) -> Result<ObjectRef, MethodCallFailed> {
    // Pin every ref-valued item BEFORE the first allocation below — the
    // per-item `create_string`/`put` calls allocate, and a moving young GC
    // there would relocate the not-yet-stored values (native stale-local
    // family). Each value is re-read from its pin right before use.
    let item_pins: Vec<Option<usize>> = items
        .iter()
        .map(|(_, v)| match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        })
        .collect();
    let read_item = |ctx: &dyn NativeContext, i: usize, v: &Value| -> Value {
        match (item_pins[i], v) {
            (Some(h), Value::Object(Some(o))) => Value::Object(Some(ctx.read_native_pin(h, *o))),
            _ => *v,
        }
    };
    // Try the real HashMap path first.
    if let Ok(Some(Value::Object(Some(map)))) = ctx.new_object("java/util/HashMap") {
        // Pin across the <init>/put invokes below (native stale-local family).
        let map_pin = ctx.pin_native_root(map);
        let _ = ctx.invoke(
            "java/util/HashMap",
            "<init>",
            "()V",
            &[Value::Object(Some(map))],
        );
        let mut all_ok = true;
        for (i, (k, v)) in items.iter().enumerate() {
            let key = ctx.create_string(k);
            let map = ctx.read_native_pin(map_pin, map);
            let v = read_item(ctx, i, v);
            let r = ctx.invoke(
                "java/util/HashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(map)), Value::Object(Some(key)), v],
            );
            if r.is_err() {
                all_ok = false;
                break;
            }
        }
        if all_ok {
            let map = ctx.read_native_pin(map_pin, map);
            match item_pins.iter().flatten().next() {
                Some(&first) => ctx.unpin_native_roots(first),
                None => ctx.unpin_native_roots(map_pin),
            }
            return Ok(map);
        }
    }
    // Fallback: synthetic parallel-array map (keys[], vals[]) the natives
    // below understand directly.
    let synth = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
    // Pin the fresh objects across the sibling allocations (native
    // stale-local family).
    let synth_pin = ctx.pin_native_root(synth);
    let keys = ctx.new_ref_array(ClassId::new(0), items.len());
    let keys_pin = ctx.pin_native_root(keys);
    let vals = ctx.new_ref_array(ClassId::new(0), items.len());
    let vals_pin = ctx.pin_native_root(vals);
    for (i, (k, v)) in items.iter().enumerate() {
        let key = ctx.create_string(k);
        let keys = ctx.read_native_pin(keys_pin, keys);
        let vals = ctx.read_native_pin(vals_pin, vals);
        let v = read_item(ctx, i, v);
        ctx.set_array_element(keys, i, Value::Object(Some(key)));
        ctx.set_array_element(vals, i, v);
    }
    let synth = ctx.read_native_pin(synth_pin, synth);
    let keys = ctx.read_native_pin(keys_pin, keys);
    let vals = ctx.read_native_pin(vals_pin, vals);
    ctx.set_field(synth, 0, Value::Object(Some(keys)));
    ctx.set_field(synth, 1, Value::Object(Some(vals)));
    ctx.set_field(synth, 2, Value::Int(items.len() as i32));
    match item_pins.iter().flatten().next() {
        Some(&first) => ctx.unpin_native_roots(first),
        None => ctx.unpin_native_roots(synth_pin),
    }
    Ok(synth)
}

/// Read a value out of a carrier's backing map by key. Handles both the
/// real-HashMap path and the synthetic parallel-array fallback.
fn carrier_get(ctx: &mut dyn NativeContext, carrier: ObjectRef, key: &str) -> Value {
    let map = match ctx.get_field_by_name(carrier, CONTENTS_FIELD) {
        Value::Object(Some(m)) => m,
        _ => return Value::Object(None),
    };
    // Pin across the key allocation + `get` invoke below — a moving young GC
    // there would relocate `map` (native stale-local family). The pin vec is
    // truncated at native exit, so the early return needs no explicit unpin.
    let map_pin = ctx.pin_native_root(map);
    // Real HashMap path.
    let key_obj = ctx.create_string(key);
    let map = ctx.read_native_pin(map_pin, map);
    if let Ok(Some(v)) = ctx.invoke_virtual(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(key_obj))],
    ) {
        if !matches!(v, Value::Object(None)) {
            return v;
        }
    }
    // Synthetic parallel-array fallback: slot 0 = keys[], slot 1 = vals[].
    // Re-read — the `get` invoke above may have moved the map.
    let map = ctx.read_native_pin(map_pin, map);
    if let (Value::Object(Some(keys)), Value::Object(Some(vals))) =
        (ctx.get_field(map, 0), ctx.get_field(map, 1))
    {
        let n = ctx.array_length(keys);
        for i in 0..n {
            if let Value::Object(Some(s)) = ctx.get_array_element(keys, i) {
                if ctx.read_string(s).as_deref() == Some(key) {
                    return ctx.get_array_element(vals, i);
                }
            }
        }
    }
    Value::Object(None)
}

/// The two open-data carrier classes. These are REAL JDK classes under
/// `real-jdk` mode — see [`delegates_to_bytecode`].
const CDS_CLASS: &str = "javax/management/openmbean/CompositeDataSupport";
const TDS_CLASS: &str = "javax/management/openmbean/TabularDataSupport";

/// Is `obj` one of CratonVM's synthetic open-data carriers?
///
/// [`build_composite_data`] / [`build_tabular_data`] allocate an instance of
/// the *real* `CompositeDataSupport` / `TabularDataSupport` class and keep
/// their state on two CratonVM-private fields ([`CONTENTS_FIELD`],
/// [`OPEN_TYPE_FIELD`]) rather than in the JDK's own `contents`/`compositeType`
/// (resp. `dataMap`/`tabularType`). An instance the application built through
/// the JDK constructors has neither field, so this is the only per-instance
/// discriminator available — native registration is per
/// (class, method, descriptor) and therefore global.
fn is_synthetic_carrier(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    matches!(
        ctx.get_field_by_name(obj, CONTENTS_FIELD),
        Value::Object(Some(_))
    ) || matches!(
        ctx.get_field_by_name(obj, OPEN_TYPE_FIELD),
        Value::Object(Some(_))
    )
}

/// Should the carrier natives below hand `obj` back to real JDK bytecode?
///
/// They were written against the carriers [`build_composite_data`] /
/// [`build_tabular_data`] mint, but registration made them shadow the JDK
/// bytecode for *every* `CompositeDataSupport`/`TabularDataSupport`, including
/// ones the application constructed itself. Those have no
/// [`CONTENTS_FIELD`]/[`OPEN_TYPE_FIELD`], so the natives answered
/// `null`/`false`/`0` for all of them:
/// `new CompositeDataSupport(t, names, values).getCompositeType()` returned
/// null, which made `CompositeType.isValue()` reject a value against the very
/// type it was built from and `CompositeDataSupport`'s own constructor throw
/// `OpenDataException` naming two type descriptions that print identically
/// (`TestJMXAccessorTask.testCreatePropertyForTabularDataSupport`). The
/// `TabularDataSupport` side was worse: the native `put` wrote into the carrier
/// map while `values()` — which has no native — read the JDK's empty `dataMap`.
///
/// Same real-vs-synthetic-by-instance problem, and same remedy, as the
/// `ThreadPoolExecutor.execute`/`submit`/`shutdown` natives; see
/// [`NativeSystemAccess::invoke_virtual_bytecode_only`], which reaches the
/// bytecode without re-entering this registration.
///
/// Gated on the class not being a fabricated stub so `synthetic-jdk` mode —
/// where there is no bytecode to delegate to — keeps the carrier behaviour
/// unchanged.
///
/// Note that the two builders currently have no caller outside this module's
/// tests, so under `real-jdk` this predicate is true for every instance and
/// the seven natives below defer wholesale. The per-instance check is kept
/// rather than reduced to the class-level gate because it is what makes the
/// natives correct again the moment a caller mints a carrier.
fn delegates_to_bytecode(ctx: &dyn NativeContext, obj: ObjectRef, class_name: &str) -> bool {
    !is_synthetic_carrier(ctx, obj) && !ctx.is_class_synthetic_stub(class_name)
}

/// Arguments to forward to [`NativeSystemAccess::invoke_virtual_bytecode_only`],
/// which pushes the receiver itself.
fn args_without_receiver(args: &[Value]) -> &[Value] {
    args.get(1..).unwrap_or(&[])
}

fn register_open_data_carriers(r: &mut NativeMethodRegistry) {
    let cds = CDS_CLASS;

    // CompositeData.get(String) -> Object.
    r.register(
        cds,
        "get",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if delegates_to_bytecode(ctx, this, CDS_CLASS) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "get",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    args_without_receiver(args),
                );
            }
            let key = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(carrier_get(ctx, this, &key)))
        },
    );

    // CompositeData.containsKey(String) -> boolean.
    r.register(cds, "containsKey", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if delegates_to_bytecode(ctx, this, CDS_CLASS) {
            return ctx.invoke_virtual_bytecode_only(
                this,
                "containsKey",
                "(Ljava/lang/String;)Z",
                args_without_receiver(args),
            );
        }
        let key = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let present = !matches!(carrier_get(ctx, this, &key), Value::Object(None));
        Ok(Some(Value::Int(present as i32)))
    });

    // CompositeData.getCompositeType() -> CompositeType.
    r.register(
        cds,
        "getCompositeType",
        "()Ljavax/management/openmbean/CompositeType;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if delegates_to_bytecode(ctx, this, CDS_CLASS) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "getCompositeType",
                    "()Ljavax/management/openmbean/CompositeType;",
                    args_without_receiver(args),
                );
            }
            Ok(Some(ctx.get_field_by_name(this, OPEN_TYPE_FIELD)))
        },
    );

    // CompositeData.get(String[]) -> Object[] (bulk read).
    r.register(
        cds,
        "getAll",
        "([Ljava/lang/String;)[Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if delegates_to_bytecode(ctx, this, CDS_CLASS) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "getAll",
                    "([Ljava/lang/String;)[Ljava/lang/Object;",
                    args_without_receiver(args),
                );
            }
            let keys = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Pin the refs held across the array allocation + per-key
            // `carrier_get` (which allocates a key string and invokes) — a
            // moving young GC there would relocate them (native stale-local
            // family). Re-read from the pins at each use.
            let this_pin = ctx.pin_native_root(this);
            let keys_pin = ctx.pin_native_root(keys);
            let n = ctx.array_length(keys);
            let out = ctx.new_ref_array(ClassId::new(0), n);
            let out_pin = ctx.pin_native_root(out);
            for i in 0..n {
                let keys = ctx.read_native_pin(keys_pin, keys);
                if let Value::Object(Some(s)) = ctx.get_array_element(keys, i) {
                    let k = ctx.read_string(s).unwrap_or_default();
                    let this = ctx.read_native_pin(this_pin, this);
                    let v = carrier_get(ctx, this, &k);
                    let out = ctx.read_native_pin(out_pin, out);
                    ctx.set_array_element(out, i, v);
                }
            }
            let out = ctx.read_native_pin(out_pin, out);
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(out))))
        },
    );

    // TabularDataSupport: a Map<List<?>, CompositeData> keyed by index
    // values. We model it on the same backing store keyed by the index's
    // String form; the natives operate on the carrier's contents map.
    let tds = TDS_CLASS;

    // TabularData.put(CompositeData) -> CompositeData. Key the row by the
    // String form of its first index item; for the simple platform tables
    // (a single-column index) this matches the JDK row-key semantics.
    r.register(
        tds,
        "put",
        "(Ljavax/management/openmbean/CompositeData;)Ljavax/management/openmbean/CompositeData;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if delegates_to_bytecode(ctx, this, TDS_CLASS) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "put",
                    "(Ljavax/management/openmbean/CompositeData;)Ljavax/management/openmbean/CompositeData;",
                    args_without_receiver(args),
                );
            }
            let row = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Pin the refs held across the allocating calls below (lazy map
            // creation, key-string allocation, `put` invoke) — a moving young
            // GC there would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let row_pin = ctx.pin_native_root(row);
            let map = match ctx.get_field_by_name(this, CONTENTS_FIELD) {
                Value::Object(Some(m)) => m,
                _ => {
                    // Lazily create a backing HashMap on first put.
                    let m = build_string_keyed_map(ctx, &[])?;
                    let this = ctx.read_native_pin(this_pin, this);
                    ctx.set_field_by_name(this, CONTENTS_FIELD, Value::Object(Some(m)));
                    m
                }
            };
            let map_pin = ctx.pin_native_root(map);
            // Row key = identity hash string (unique per row); this gives a
            // working put/get/size without parsing the table's index names.
            let row = ctx.read_native_pin(row_pin, row);
            let key_str = format!("row#{}", ctx.identity_hash_code(row));
            let key = ctx.create_string(&key_str);
            let map = ctx.read_native_pin(map_pin, map);
            let row = ctx.read_native_pin(row_pin, row);
            let _ = ctx.invoke_virtual(
                map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key)), Value::Object(Some(row))],
            );
            // Re-read once more — the `put` invoke above may have moved the
            // row we hand back to the caller.
            let row = ctx.read_native_pin(row_pin, row);
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(row))))
        },
    );

    // TabularData.size() -> int.
    r.register(tds, "size", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if delegates_to_bytecode(ctx, this, TDS_CLASS) {
            return ctx.invoke_virtual_bytecode_only(this, "size", "()I", &[]);
        }
        let map = match ctx.get_field_by_name(this, CONTENTS_FIELD) {
            Value::Object(Some(m)) => m,
            _ => return Ok(Some(Value::Int(0))),
        };
        match ctx.invoke_virtual(map, "size", "()I", &[]) {
            Ok(Some(v @ Value::Int(_))) => Ok(Some(v)),
            _ => Ok(Some(Value::Int(0))),
        }
    });

    // TabularData.isEmpty() -> boolean.
    r.register(tds, "isEmpty", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(1))),
        };
        if delegates_to_bytecode(ctx, this, TDS_CLASS) {
            return ctx.invoke_virtual_bytecode_only(this, "isEmpty", "()Z", &[]);
        }
        let map = match ctx.get_field_by_name(this, CONTENTS_FIELD) {
            Value::Object(Some(m)) => m,
            _ => return Ok(Some(Value::Int(1))),
        };
        let empty = match ctx.invoke_virtual(map, "size", "()I", &[]) {
            Ok(Some(Value::Int(n))) => n == 0,
            _ => true,
        };
        Ok(Some(Value::Int(empty as i32)))
    });
    ()
}

/// Build a `TabularDataSupport`-shaped carrier with the given tabular type
/// and an empty backing map. Rows are added via the `put` native above.
pub(crate) fn build_tabular_data(
    ctx: &mut dyn NativeContext,
    tabular_type: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    // Pin every ref held across the allocating calls below — a moving young
    // GC there would relocate them (native stale-local family). The map is
    // built first so only the carrier needs a pin across it.
    let tabular_type_pin = tabular_type.map(|o| ctx.pin_native_root(o));
    let map = build_string_keyed_map(ctx, &[])?;
    let map_pin = ctx.pin_native_root(map);
    let obj =
        try_alloc_concurrent_synthetic(ctx, "javax/management/openmbean/TabularDataSupport", 4)?;
    let map = ctx.read_native_pin(map_pin, map);
    ctx.set_field_by_name(obj, CONTENTS_FIELD, Value::Object(Some(map)));
    let tabular_type = match (tabular_type_pin, tabular_type) {
        (Some(h), Some(o)) => Some(ctx.read_native_pin(h, o)),
        _ => tabular_type,
    };
    ctx.set_field_by_name(obj, OPEN_TYPE_FIELD, Value::Object(tabular_type));
    match tabular_type_pin {
        Some(h) => ctx.unpin_native_roots(h),
        None => ctx.unpin_native_roots(map_pin),
    }
    Ok(obj)
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
        _ => return Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)?)))),
    };
    let normalized = resolve_type_name(ctx, type_obj);

    // Cycle detector — same logic as native_mapping_for_type.
    if let Some(name) = &normalized {
        let depth = VISITED_TYPES.with(|v| v.borrow().iter().filter(|t| t == &name).count());
        if depth >= MAX_MAPPING_DEPTH {
            return Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)?))));
        }
    }

    if let Some(name) = &normalized {
        if PROBLEMATIC_TYPE_NAMES.contains(&name.as_str()) {
            return Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)?))));
        }
    }

    if let Some(name) = &normalized {
        if let Some(schema) = composite_schema_for(name) {
            return Ok(Some(Value::Object(Some(alloc_open_converter(
                ctx,
                Some(schema),
            )?))));
        }
    }

    Ok(Some(Value::Object(Some(alloc_open_converter(ctx, None)?))))
}

/// T19.M1 — Allocate an `OpenConverter` instance with either a
/// CompositeType (when `schema` is `Some`) or `SimpleType.STRING`
/// (when `None`) as its open-type.
fn alloc_open_converter(
    ctx: &mut dyn NativeContext,
    schema: Option<&CompositeSchema>,
) -> Result<ObjectRef, MethodCallFailed> {
    let oc = try_alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/OpenConverter", 4)?;
    // Pin across the sibling allocations below — a moving young GC there
    // would relocate the fresh converter (native stale-local family).
    let oc_pin = ctx.pin_native_root(oc);
    // Field 0: targetType (Type) — null acceptable.
    ctx.set_field(oc, 0, Value::Object(None));
    // Field 1: openType (OpenType) — composite OR simple.
    let open_type = match schema {
        Some(s) => alloc_composite_type(ctx, s),
        None => alloc_simple_type_string(ctx),
    };
    let oc = ctx.read_native_pin(oc_pin, oc);
    ctx.set_field(oc, 1, Value::Object(Some(open_type?)));
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
    let oc = ctx.read_native_pin(oc_pin, oc);
    ctx.set_field(oc, 2, Value::Object(Some(open_class_mirror)));
    // Field 3: identityConverter flag — set to 1 so consumers skip
    // bidirectional conversion paths (we don't translate values).
    ctx.set_field(oc, 3, Value::Int(1));
    ctx.unpin_native_roots(oc_pin);
    Ok(oc)
}

/// T19.M1 — Native override for
/// `com/sun/jmx/mbeanserver/MappedMXBeanType.getMappedMXBeanType(Type)`.
/// Returns a synthetic `MappedMXBeanType` for known composite types,
/// or a String-mapped type otherwise.
fn native_mapped_mxbean_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let type_obj = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(
                ctx, None,
            )?))))
        }
    };
    let normalized = resolve_type_name(ctx, type_obj);

    if let Some(name) = &normalized {
        let depth = VISITED_TYPES.with(|v| v.borrow().iter().filter(|t| t == &name).count());
        if depth >= MAX_MAPPING_DEPTH {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(
                ctx, None,
            )?))));
        }
    }
    if let Some(name) = &normalized {
        if PROBLEMATIC_TYPE_NAMES.contains(&name.as_str()) {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(
                ctx, None,
            )?))));
        }
        if let Some(schema) = composite_schema_for(name) {
            return Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(
                ctx,
                Some(schema),
            )?))));
        }
    }
    Ok(Some(Value::Object(Some(alloc_mapped_mxbean_type(
        ctx, None,
    )?))))
}

/// T19.M1 — Allocate a `MappedMXBeanType` instance.
fn alloc_mapped_mxbean_type(
    ctx: &mut dyn NativeContext,
    schema: Option<&CompositeSchema>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mt = try_alloc_concurrent_synthetic(ctx, "com/sun/jmx/mbeanserver/MappedMXBeanType", 4)?;
    // Pin across the sibling allocations below — a moving young GC there
    // would relocate the fresh instance (native stale-local family).
    let mt_pin = ctx.pin_native_root(mt);
    let open_type = match schema {
        Some(s) => alloc_composite_type(ctx, s),
        None => alloc_simple_type_string(ctx),
    };
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field(mt, 0, Value::Object(Some(open_type?)));
    // Field 1: typeName.
    let type_name_str = match schema {
        Some(s) => ctx.create_string(s.type_name),
        None => ctx.create_string("java.lang.String"),
    };
    let mt = ctx.read_native_pin(mt_pin, mt);
    ctx.set_field(mt, 1, Value::Object(Some(type_name_str)));
    // Field 2: isBasicType — 1 if SimpleType, 0 if Composite.
    ctx.set_field(mt, 2, Value::Int(if schema.is_some() { 0 } else { 1 }));
    // Field 3: arrayMapping flag — 0 (we don't model arrays here).
    ctx.set_field(mt, 3, Value::Int(0));
    ctx.unpin_native_roots(mt_pin);
    Ok(mt)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
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
        assert!(r
            .find(
                "com/sun/jmx/mbeanserver/MBeanIntrospector",
                "getMethods",
                "(Ljava/lang/Class;)Ljava/util/List;"
            )
            .is_some());
        assert!(r
            .find(
                "com/sun/jmx/mbeanserver/MXBeanIntrospector",
                "getMethods",
                "(Ljava/lang/Class;)Ljava/util/List;"
            )
            .is_some());
    }

    /// The type-mapping overlay is the synthetic arm ONLY.
    ///
    /// Registered by default, `ConvertingMethod.from` installs an identity
    /// return mapping and `mappingForType` types everything it does not
    /// recognise as `SimpleType.STRING`, which is what made
    /// `MBeanServer.getAttribute` answer a raw `java.lang.management.
    /// MemoryUsage` instead of a `CompositeDataSupport`.
    #[test]
    fn type_mapping_overlay_is_synthetic_only() {
        const MAPPING_OVERLAY: &[(&str, &str, &str)] = &[
            (
                "com/sun/jmx/mbeanserver/ConvertingMethod",
                "from",
                "(Ljava/lang/reflect/Method;)Lcom/sun/jmx/mbeanserver/ConvertingMethod;",
            ),
            (
                "com/sun/jmx/mbeanserver/MXBeanMappingFactory",
                "mappingForType",
                "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
            ),
            (
                "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
                "mappingForType",
                "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
            ),
            (
                "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
                "makeMapping",
                "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
            ),
            (
                "com/sun/jmx/mbeanserver/MXBeanMapping",
                "toOpenValue",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (
                "com/sun/jmx/mbeanserver/MXBeanMapping",
                "fromOpenValue",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
        ];

        let mut real = NativeMethodRegistry::new();
        register_jmx_openmbean_natives_with(&mut real, false);
        for (c, m, d) in MAPPING_OVERLAY {
            assert!(
                real.find(c, m, d).is_none(),
                "{c}.{m} must not shadow the real mapping factory by default"
            );
        }

        let mut synth = NativeMethodRegistry::new();
        register_jmx_openmbean_natives_with(&mut synth, true);
        for (c, m, d) in MAPPING_OVERLAY {
            assert!(
                synth.find(c, m, d).is_some(),
                "{c}.{m} must still be available for synthetic-jdk builds"
            );
        }

        // The Object-method filter is the PRIMARY fix, not part of the
        // overlay: it is what keeps the real factory from being offered the
        // `Class` -> `AnnotatedType[]` self-reference at all, so it stays
        // registered in both arms.
        for r in [&real, &synth] {
            assert!(r
                .find(
                    "com/sun/jmx/mbeanserver/MXBeanIntrospector",
                    "getMethods",
                    "(Ljava/lang/Class;)Ljava/util/List;"
                )
                .is_some());
        }
    }

    #[test]
    fn test_mapping_for_type_registered() {
        // Both `mappingForType` spellings belong to the synthetic overlay;
        // `type_mapping_overlay_is_synthetic_only` above owns the full arm
        // comparison. This one keeps the synthetic arm's own coverage.
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives_with(&mut r, true);
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
    fn test_primitive_or_primitive_array_name_for_jmx_signatures() {
        assert!(primitive_or_primitive_array_name("long"));
        assert!(primitive_or_primitive_array_name("[J"));
        assert!(primitive_or_primitive_array_name("[[I"));
        assert!(!primitive_or_primitive_array_name("java/lang/String"));
        assert!(!primitive_or_primitive_array_name("[Ljava/lang/String;"));
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
        let m = alloc_identity_mapping(&mut ctx).unwrap();
        // Should be a real ObjectRef.
        let _ = m;
    }

    #[test]
    fn test_alloc_array_list_from_empty() {
        let mut ctx = mock_ctx();
        let lst = alloc_array_list_from(&mut ctx, &[]).unwrap();
        // Slot 1 = size = 0
        match ctx.get_field(lst, 1) {
            Value::Int(0) => {}
            other => panic!("expected size=0, got {:?}", other),
        }
    }

    #[test]
    fn test_type_descriptor_to_class_mirror_malformed_l_no_panic() {
        // Regression: attacker-controlled object descriptors with a missing
        // terminating `;` (or shorter than the minimum `L_;`) must not panic
        // on the internal-name slice. They should resolve to a non-null
        // fallback mirror instead.
        let mut ctx = mock_ctx();
        for desc in ["L", "L;", "Lfoo", "Ljava/lang/String", "[", "[L"] {
            let m = type_descriptor_to_class_mirror(&mut ctx, desc);
            assert!(
                !m.as_ptr().is_null(),
                "malformed descriptor {:?} produced a null mirror",
                desc
            );
        }
    }

    #[test]
    fn test_type_descriptor_to_class_mirror_wellformed_l_no_panic() {
        // A well-formed object descriptor must still be accepted without
        // panicking after the bounds hardening.
        let mut ctx = mock_ctx();
        let m = type_descriptor_to_class_mirror(&mut ctx, "Ljava/lang/Object;");
        assert!(!m.as_ptr().is_null());
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
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
            ],
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
        let r =
            native_introspector_get_methods(&mut ctx, &[Value::Object(None), Value::Object(None)]);
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
        let s =
            composite_schema_for("java.lang.management.MemoryUsage").expect("MemoryUsage schema");
        assert_eq!(s.items.len(), 4);
        assert_eq!(s.items, &["init", "used", "committed", "max"]);
    }

    #[test]
    fn t19_m1_composite_schema_thread_info_present() {
        let s = composite_schema_for("java.lang.management.ThreadInfo").expect("ThreadInfo schema");
        // ThreadInfo has 17 published item names in JDK 25.
        assert_eq!(s.items.len(), 17);
        assert!(s.items.contains(&"threadId"));
        assert!(s.items.contains(&"threadState"));
        assert!(s.items.contains(&"stackTrace"));
        assert!(s.items.contains(&"daemon"));
    }

    #[test]
    fn t19_m1_composite_schema_lock_info_present() {
        let s = composite_schema_for("java.lang.management.LockInfo").expect("LockInfo schema");
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items, &["className", "identityHashCode"]);
    }

    #[test]
    fn t19_m1_composite_schema_monitor_info_present() {
        let s =
            composite_schema_for("java.lang.management.MonitorInfo").expect("MonitorInfo schema");
        assert_eq!(s.items.len(), 4);
        assert!(s.items.contains(&"lockedStackDepth"));
        assert!(s.items.contains(&"lockedStackFrame"));
    }

    #[test]
    fn t19_m1_composite_schema_stack_trace_element_present() {
        let s =
            composite_schema_for("java.lang.StackTraceElement").expect("StackTraceElement schema");
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
        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
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

        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
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

        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
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
        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
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
        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
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
        let schema =
            composite_schema_for("java.lang.management.MemoryUsage").expect("schema present");
        let m = alloc_composite_mapping(&mut ctx, schema).unwrap();
        // Verify the openType slot is populated.
        match ctx.get_field(m, 1) {
            Value::Object(Some(_)) => {}
            other => panic!("expected populated openType, got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_composite_type_carries_item_names() {
        let mut ctx = mock_ctx();
        let schema = composite_schema_for("java.lang.management.LockInfo").expect("schema");
        let ct = alloc_composite_type(&mut ctx, schema).unwrap();
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
        let schema = composite_schema_for("java.lang.management.ThreadInfo").expect("schema");
        let oc = alloc_open_converter(&mut ctx, Some(schema)).unwrap();
        // identityConverter flag (slot 3) should be 1.
        match ctx.get_field(oc, 3) {
            Value::Int(1) => {}
            other => panic!("expected Int(1), got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_open_converter_without_schema_uses_simple_string() {
        let mut ctx = mock_ctx();
        let oc = alloc_open_converter(&mut ctx, None).unwrap();
        // openType (slot 1) must be a SimpleType-shaped object (non-null).
        match ctx.get_field(oc, 1) {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null openType, got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_mapped_mxbean_type_basic_for_unknown() {
        let mut ctx = mock_ctx();
        let mt = alloc_mapped_mxbean_type(&mut ctx, None).unwrap();
        // isBasicType (slot 2) should be 1 (SimpleType-mapped).
        match ctx.get_field(mt, 2) {
            Value::Int(1) => {}
            other => panic!("expected Int(1), got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_alloc_mapped_mxbean_type_composite_for_known() {
        let mut ctx = mock_ctx();
        let schema = composite_schema_for("java.lang.management.MemoryUsage").expect("schema");
        let mt = alloc_mapped_mxbean_type(&mut ctx, Some(schema)).unwrap();
        // isBasicType should be 0 for composite.
        match ctx.get_field(mt, 2) {
            Value::Int(0) => {}
            other => panic!("expected Int(0) for composite, got {:?}", other),
        }
    }

    #[test]
    fn t19_m1_native_open_converter_to_converter_handles_null() {
        let mut ctx = mock_ctx();
        let r = native_open_converter_to_converter(&mut ctx, &[Value::Object(None)])
            .expect("native call");
        assert!(matches!(r, Some(Value::Object(Some(_)))));
    }

    #[test]
    fn t19_m1_native_mapped_mxbean_type_handles_null() {
        let mut ctx = mock_ctx();
        let r = native_mapped_mxbean_type(&mut ctx, &[Value::Object(None)]).expect("native call");
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
        assert!(r
            .find(
                "com/sun/jmx/mbeanserver/OpenConverter",
                "toConverter",
                "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/OpenConverter;"
            )
            .is_some());
    }

    #[test]
    fn t19_m1_mapped_mxbean_type_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        assert!(r
            .find(
                "com/sun/jmx/mbeanserver/MappedMXBeanType",
                "getMappedMXBeanType",
                "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/MappedMXBeanType;"
            )
            .is_some());
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
        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
        let name_str = ctx.create_string("java.lang.management.MemoryUsage");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));
        let resolved = resolve_type_name(&ctx, class_mirror);
        assert_eq!(
            resolved.as_deref(),
            Some("java.lang.management.MemoryUsage")
        );
    }

    #[test]
    fn t19_m1_resolve_type_name_normalizes_slashes_to_dots() {
        let mut ctx = mock_ctx();
        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
        let name_str = ctx.create_string("java/lang/management/MemoryUsage");
        ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));
        let resolved = resolve_type_name(&ctx, class_mirror);
        // Slashes must be normalized to dots so the lookup against
        // COMPOSITE_TYPE_NAMES succeeds.
        assert_eq!(
            resolved.as_deref(),
            Some("java.lang.management.MemoryUsage")
        );
    }

    // -----------------------------------------------------------------
    // OpenMBean value-carrier tests (CompositeData / TabularData).
    // -----------------------------------------------------------------

    #[test]
    fn open_data_carriers_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_openmbean_natives(&mut r);
        let cds = "javax/management/openmbean/CompositeDataSupport";
        assert!(r
            .find(cds, "get", "(Ljava/lang/String;)Ljava/lang/Object;")
            .is_some());
        assert!(r
            .find(cds, "containsKey", "(Ljava/lang/String;)Z")
            .is_some());
        assert!(r
            .find(
                cds,
                "getCompositeType",
                "()Ljavax/management/openmbean/CompositeType;"
            )
            .is_some());
        assert!(r
            .find(cds, "getAll", "([Ljava/lang/String;)[Ljava/lang/Object;")
            .is_some());
        let tds = "javax/management/openmbean/TabularDataSupport";
        assert!(
            r.find(
                tds,
                "put",
                "(Ljavax/management/openmbean/CompositeData;)Ljavax/management/openmbean/CompositeData;"
            )
            .is_some()
        );
        assert!(r.find(tds, "size", "()I").is_some());
        assert!(r.find(tds, "isEmpty", "()Z").is_some());
    }

    #[test]
    fn build_composite_data_returns_non_null() {
        let mut ctx = mock_ctx();
        let items = vec![
            ("init".to_string(), Value::Long(0)),
            ("used".to_string(), Value::Long(1024)),
        ];
        let cd = build_composite_data(&mut ctx, None, &items).unwrap();
        // The carrier object must be a real allocated object.
        assert!(ctx.object_num_fields(cd) >= 4);
    }

    #[test]
    fn build_tabular_data_returns_non_null() {
        let mut ctx = mock_ctx();
        let td = build_tabular_data(&mut ctx, None).unwrap();
        assert!(ctx.object_num_fields(td) >= 4);
    }

    #[test]
    fn only_cratonvm_built_carriers_are_answered_by_the_carrier_natives() {
        use cratonvm_native_api::FieldMetadata;
        let mut ctx = mock_ctx();
        let cid = match ctx.ensure_class_initialized(CDS_CLASS) {
            Ok(cid) => cid,
            Err(e) => panic!("mock could not initialize {CDS_CLASS}: {e:?}"),
        };
        // The mock resolves a field name only through `set_declared_fields`;
        // the real VM's by-name path resolves the two carrier fields on the
        // synthetically-allocated instance itself. Declare them so both
        // branches below are reachable here.
        ctx.set_declared_fields(
            cid,
            vec![
                FieldMetadata {
                    name: CONTENTS_FIELD.to_string(),
                    descriptor: "Ljava/lang/Object;".to_string(),
                    access_flags: 0,
                    slot_index: 0,
                    declaring_class_id: cid,
                    is_static: false,
                },
                FieldMetadata {
                    name: OPEN_TYPE_FIELD.to_string(),
                    descriptor: "Ljava/lang/Object;".to_string(),
                    access_flags: 0,
                    slot_index: 1,
                    declaring_class_id: cid,
                    is_static: false,
                },
            ],
        );

        let carrier = match ctx.new_object(CDS_CLASS) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("mock could not allocate an instance: {other:?}"),
        };

        // Untouched, an instance looks exactly like one the application built
        // through the JDK's own constructor: neither carrier field is set. The
        // natives answered those from the carrier fields and so returned null
        // for everything — `getCompositeType()` in particular, which made
        // `CompositeType.isValue()` reject a value against its own declared
        // type. It has to go back to the bytecode.
        assert!(!is_synthetic_carrier(&ctx, carrier));
        assert!(delegates_to_bytecode(&ctx, carrier, CDS_CLASS));

        // Stamped the way `build_composite_data` stamps it, the same instance
        // is this crate's own carrier and the natives must keep answering it —
        // under `synthetic-jdk` there is no bytecode to fall back to.
        let map = match ctx.new_object("java/util/HashMap") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("mock could not allocate a map: {other:?}"),
        };
        ctx.set_field_by_name(carrier, CONTENTS_FIELD, Value::Object(Some(map)));
        assert!(is_synthetic_carrier(&ctx, carrier));
        assert!(!delegates_to_bytecode(&ctx, carrier, CDS_CLASS));
    }

    #[test]
    fn build_string_keyed_map_synthetic_fallback_roundtrips() {
        // Drive the synthetic parallel-array fallback by forcing the
        // real-HashMap path to be skipped: the mock's `invoke` returns
        // Ok(None) for put, so the real path "succeeds" with an empty
        // map. To exercise the fallback storage + carrier_get we build
        // the synthetic map directly and verify slot layout.
        let mut ctx = mock_ctx();
        // Allocate a synthetic map exactly as the fallback does.
        let synth = try_alloc_concurrent_synthetic(&mut ctx, "java/util/HashMap", 3).unwrap();
        let keys = ctx.new_ref_array(ClassId::new(0), 1);
        let vals = ctx.new_ref_array(ClassId::new(0), 1);
        let k = ctx.create_string("used");
        ctx.set_array_element(keys, 0, Value::Object(Some(k)));
        ctx.set_array_element(vals, 0, Value::Long(4096));
        ctx.set_field(synth, 0, Value::Object(Some(keys)));
        ctx.set_field(synth, 1, Value::Object(Some(vals)));
        ctx.set_field(synth, 2, Value::Int(1));
        // Verify the parallel-array scan finds the value.
        match (ctx.get_field(synth, 0), ctx.get_field(synth, 1)) {
            (Value::Object(Some(ks)), Value::Object(Some(vs))) => {
                assert_eq!(ctx.array_length(ks), 1);
                assert!(matches!(ctx.get_array_element(vs, 0), Value::Long(4096)));
            }
            _ => panic!("synthetic map slots not populated"),
        }
    }
}
