// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Direct implementations of the javac-generated record `hashCode`/`equals`
//! bodies (JEP 395).
//!
//! Unlike the other modules here these are not delegates to a registered
//! native: a record's `hashCode`/`equals` is a bare `invokedynamic` against
//! `java.lang.runtime.ObjectMethods.bootstrap`, which the VM has always
//! serviced from Rust rather than by materialising the bootstrap's
//! `MethodHandle` chain. This module is that implementation, and it is the
//! **single** source of truth: the interpreter's `invokedynamic` call-site
//! executor (`vm::runtime::invokedynamic`) delegates here too, so a record's
//! hash cannot depend on which path served the call.
//!
//! # Why an intrinsic
//!
//! The generated body cannot be JIT-compiled — the x64 backend lowers
//! `invokedynamic` to an unconditional deopt, so the method reverts to
//! interpreter-only execution for the life of the process. Every `HashMap` /
//! `HashSet` probe keyed by a record then pays an interpreter frame plus a
//! call-site-cache lookup on top of the hashing itself, which measured ~3 µs
//! per `hashCode` against HotSpot's ~3 ns. Hibernate ORM 8's graph flush
//! planner keys its dependency graph on records (`GroupNode`,
//! `FlushOperationGroup`, `StatementShapeKey`) and hashes them millions of
//! times per flush, turning a sub-second HotSpot plan into a multi-minute
//! hang. Installing these as interpreter intrinsics removes the frame, the
//! `invokedynamic` dispatch and the call-site clone.
//!
//! # Nested records
//!
//! A reference component is hashed/compared through its VIRTUAL
//! `hashCode`/`equals` (the JDK's generated body uses `Objects.hashCode` /
//! `Objects.equals`). Two shapes are recognised natively so a nested record or
//! a `String` component costs no Java dispatch at all:
//!
//! * `java.lang.String` — via the VM's compact string reader.
//! * a record whose corresponding object method is itself javac-generated —
//!   recursing directly into this module.
//! * an enum — JLS §8.9 makes `Enum.hashCode`/`Enum.equals` final and
//!   identity-based, so the hash comes from the same `Object.hashCode` native
//!   the dispatch would have reached and the comparison is reference equality.
//!
//! * a `java.util.ArrayList` — see [`array_list_hash`]. `List.hashCode`/`equals`
//!   are fixed by the `java.util.List` interface contract, so walking the
//!   backing array natively and recursing per element is faithful, and it turns
//!   ~50 Java dispatches per component into ~50 native string hashes.
//!
//! * a boxed primitive (`Long`, `Integer`, ...), under
//!   `CRATONVM_RECORD_BOXED_COMPONENTS` (default ON since i7 wave 144, `=0` off) — see [`boxed_hash`].
//!   With the switch off its own final `hashCode`/`equals` is called directly:
//!   it neither throws nor looks at the stack.
//!
//! # Frames (JIT round 21 wave 19, lane recnojit)
//!
//! Anything else (other collections, ordinary classes) runs Java that can
//! throw or walk the stack, and HotSpot shows `Objects.hashCode` /
//! `Objects.equals` and the record's own method below it. So such a component
//! is never called from the intrinsic: the intrinsic answers only frame-free
//! folds and otherwise runs the record's real body; the `invokedynamic`
//! executor (inside that body's frame) calls the component through
//! `java.util.Objects`. See [`Calls`]
//! (`docs/internal/fixed-bugs/jit/jr21-records-nojit-component-frames-FIXED-20261026.md`).
//!
//! Recursion depth is bounded by [`MAX_NESTING`]; past that the component
//! takes the Java path, which cannot loop forever because a record
//! component graph that deep is already a `StackOverflowError` on HotSpot.

/// Bit 0 of [`NativeContext::object_method_fast_path`]'s mask.
const GENERATED_HASH_CODE: u8 = 1 << 0;
/// Bit 1 of [`NativeContext::object_method_fast_path`]'s mask.
const GENERATED_EQUALS: u8 = 1 << 1;
/// Bit 3 of [`NativeContext::object_method_fast_path`]'s mask — identity
/// `hashCode`/`equals` (enum classes).
const IDENTITY_SEMANTICS: u8 = 1 << 3;
/// Bit 4 of [`NativeContext::object_method_fast_path`]'s mask — `java.lang.String`.
const IS_STRING: u8 = 1 << 4;
/// Bit 5 of [`NativeContext::object_method_fast_path`]'s mask — exactly
/// `java.util.ArrayList`.
const IS_ARRAY_LIST: u8 = 1 << 5;
/// Bit 6 of [`NativeContext::object_method_fast_path`]'s mask — one of the
/// eight boxed primitives; the answer's `.1` is its `value` slot. Final
/// classes whose `hashCode`/`equals` neither throw nor walk the stack, so the
/// frame-free fold may answer or call them (JIT round 21 wave 19, lane
/// recnojit).
const BOXED_PRIMITIVE: u8 = 1 << 6;
/// Bit 7 of [`NativeContext::object_method_fast_path`]'s mask — with bit 6,
/// `java.lang.Boolean`.
const BOXED_BOOLEAN: u8 = 1 << 7;

/// Native-recursion depth cap for nested record components.
const MAX_NESTING: u32 = 64;

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ObjectRef, Value};

/// How a fold treats a reference component whose `hashCode` / `equals` is
/// not one of the frame-free shapes ([`reference_hash_without_frames`],
/// [`references_equal_without_frames`]).
///
/// JIT round 21 wave 19, lane recnojit
/// (`jr21-records-nojit-component-frames-FIXED-20261026.md`): such a component runs
/// Java, and that Java can throw or walk the stack. HotSpot's frames below it
/// are `Objects.hashCode` / `Objects.equals` (the handle chain's per-component
/// call) and then the record's own `hashCode` / `equals`. The interpreter
/// intrinsic has no frame for the record method, and the native fold called
/// the component's method directly, so a throwing component showed
/// `Boom.hashCode < <caller>` instead of
/// `Boom.hashCode < Objects.hashCode < Two.hashCode < <caller>`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Calls {
    /// Answer `None` instead of calling: the caller (the intrinsic) runs the
    /// record's real body, which brings the record frame.
    Refuse,
    /// Call it through `java.util.Objects` (a real frame): the
    /// `invokedynamic` executor, which already runs inside the record
    /// method's frame.
    ThroughObjects,
}

/// Intrinsic for a record's generated `hashCode ()I` (virtual).
///
/// `args == [receiver]`. Answers natively only when no component needs a
/// Java call a throw or a stack walk could see ([`Calls::Refuse`]);
/// otherwise it runs the record's own body (`invokedynamic` and all), so the
/// frames are HotSpot's. Correctness fix, unconditional (JIT round 21 wave
/// 19, lane recnojit).
pub fn intrinsic_record_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // A null receiver never reaches a virtual call site (the interpreter
        // raises the NPE first); mirror the invokedynamic executor's `0`.
        _ => return Ok(Some(Value::Int(0))),
    };
    // A boxed component's `hashCode` may run Java (and so collect) inside
    // the frame-free fold: keep the receiver for the fallback call.
    let pin = ctx.pin_native_root(this);
    let without_frames = record_hash_code_at(ctx, this, 0, Calls::Refuse);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    if let Some(hash) = without_frames? {
        return Ok(Some(Value::Int(hash)));
    }
    ctx.invoke_virtual(this, "hashCode", "()I", &[])
}

/// Intrinsic for a record's generated `equals (Ljava/lang/Object;)Z` (virtual).
///
/// `args == [receiver, other]`. Same contract as
/// [`intrinsic_record_hash_code`].
pub fn intrinsic_record_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // `x.equals(null)` and `x.equals(<primitive>)` are both false.
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let without_frames = record_equals_at(ctx, this, other, 0, Calls::Refuse);
    let this = ctx.read_native_pin(this_pin, this);
    let other = ctx.read_native_pin(other_pin, other);
    ctx.unpin_native_roots(other_pin);
    ctx.unpin_native_roots(this_pin);
    if let Some(equal) = without_frames? {
        return Ok(Some(Value::Int(i32::from(equal))));
    }
    ctx.invoke_virtual(
        this,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(other))],
    )
}

/// `hashCode` for a record instance: `h = 0; h = h * 31 + hash(component)` in
/// component declaration order, matching what the JDK's `ObjectMethods`
/// bootstrap produces. For the `invokedynamic` executor, which runs inside
/// the record method's frame: a component that needs Java is called through
/// `Objects.hashCode` ([`Calls::ThroughObjects`]).
pub fn record_hash_code(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    record_hash_code_at(ctx, this, 0, Calls::ThroughObjects).map(|hash| hash.unwrap_or(0))
}

/// `equals` for a record instance: same class, then every component compared
/// with the JDK's per-component semantics (primitives by value with
/// `Float`/`Double` bit equality, references via `Objects.equals`). For the
/// `invokedynamic` executor, as [`record_hash_code`].
pub fn record_equals(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    record_equals_at(ctx, this, other, 0, Calls::ThroughObjects).map(|equal| equal.unwrap_or(false))
}

/// `hashCode` of an `ObjectMethods` site whose getters are NOT javac's shape
/// (a subset of the components, another order): `h = 31 * h + hash(getter)`
/// over the SITE's getters, in the site's order, as `ObjectMethods.makeHashCode`
/// folds them. Getter `i` reads instance slot `slots[i]` and hashes by
/// `descriptors[i]` (its field's type), so a `boolean` getter hashes
/// `1231`/`1237`. Interpreter round i1 wave 39, lane L4
/// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`).
pub fn record_hash_code_by_getters(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slots: &[usize],
    descriptors: &[std::sync::Arc<str>],
) -> Result<i32, MethodCallFailed> {
    let pin = ctx.pin_native_root(this);
    let mut hash: Result<i32, MethodCallFailed> = Ok(0);
    for (i, &slot) in slots.iter().enumerate() {
        let current = ctx.read_native_pin(pin, this);
        let component = ctx.get_field(current, slot);
        let hashed = match (component, descriptors.get(i).map(|d| &**d)) {
            (Value::Int(v), Some("Z")) => Ok(if v != 0 {
                BOOLEAN_HASH_TRUE
            } else {
                BOOLEAN_HASH_FALSE
            }),
            _ => component_hash(ctx, &component, 0, Calls::ThroughObjects)
                .map(|hash| hash.unwrap_or(0)),
        };
        match hashed {
            Ok(component_hash) => {
                hash = Ok(hash
                    .unwrap_or(0)
                    .wrapping_mul(31)
                    .wrapping_add(component_hash));
            }
            Err(failure) => {
                hash = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    hash
}

/// `equals` of an `ObjectMethods` site whose getters are NOT javac's shape,
/// as `ObjectMethods.makeEquals` builds it: the same object is equal; an
/// `other` that is not an instance of `record_class` (`Class.isInstance`) is
/// not; otherwise the SITE's getters are compared, the LAST one first (each
/// new `guardWithTest` is outermost), stopping at the first difference. See
/// [`record_hash_code_by_getters`] for `slots`.
pub fn record_equals_by_getters(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
    record_class: cratonvm_types::ClassId,
    slots: &[usize],
) -> Result<bool, MethodCallFailed> {
    if this == other {
        return Ok(true);
    }
    // A reference array reports its component class id (see
    // `record_equals_at`); an array is never an instance of a record class.
    if ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object
        || !ctx.is_subclass(ctx.class_id_of_object(other), record_class)
    {
        return Ok(false);
    }
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let mut equal: Result<bool, MethodCallFailed> = Ok(true);
    for &slot in slots.iter().rev() {
        let this_current = ctx.read_native_pin(this_pin, this);
        let other_current = ctx.read_native_pin(other_pin, other);
        let left = ctx.get_field(this_current, slot);
        let right = ctx.get_field(other_current, slot);
        match components_equal(ctx, &left, &right, 0, Calls::ThroughObjects) {
            Ok(Some(true)) => continue,
            Ok(_) => {
                equal = Ok(false);
                break;
            }
            Err(failure) => {
                equal = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(other_pin);
    ctx.unpin_native_roots(this_pin);
    equal
}

/// The record fold's `hashCode` over `this`'s components. `Ok(None)` only
/// under [`Calls::Refuse`]: a component needs a Java call.
fn record_hash_code_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    depth: u32,
    calls: Calls,
) -> Result<Option<i32>, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(this);
    let (_, first_slot, num_components) = object_method_fast_path_of(ctx, class_id);

    // The component reads themselves cannot move the heap, but hashing a
    // reference component may call back into Java and therefore GC. Pin the
    // receiver and re-read it per component so a moved object is followed.
    let pin = ctx.pin_native_root(this);
    let mut hash: Result<Option<i32>, MethodCallFailed> = Ok(Some(0));
    for index in first_slot..first_slot + num_components {
        let current = ctx.read_native_pin(pin, this);
        let component = ctx.get_field(current, index);
        // G9-1: a `boolean` component hashes 1231/1237, not 1/0. The class
        // lookup is behind the `0 | 1` guard because those are the only two
        // values at which `Boolean.hashCode` and `Integer.hashCode` differ,
        // and a `boolean` field cannot hold any other.
        let mut boolean_hash: Option<i32> = None;
        if let Value::Int(v) = component {
            if (v == 0 || v == 1) && is_boolean_component(ctx, class_id, index - first_slot) {
                boolean_hash = Some(if v == 1 {
                    BOOLEAN_HASH_TRUE
                } else {
                    BOOLEAN_HASH_FALSE
                });
            }
        }
        let hashed = match boolean_hash {
            Some(h) => Ok(Some(h)),
            None => component_hash(ctx, &component, depth, calls),
        };
        match hashed {
            Ok(Some(component_hash)) => {
                hash = Ok(Some(
                    hash.ok()
                        .flatten()
                        .unwrap_or(0)
                        .wrapping_mul(31)
                        .wrapping_add(component_hash),
                ));
            }
            Ok(None) => {
                hash = Ok(None);
                break;
            }
            Err(failure) => {
                hash = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    hash
}

/// The record fold's `equals`. `Ok(None)` only under [`Calls::Refuse`]: a
/// component needs a Java call before any difference was found.
fn record_equals_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
    depth: u32,
    calls: Calls,
) -> Result<Option<bool>, MethodCallFailed> {
    if this == other {
        return Ok(Some(true));
    }
    let this_class = ctx.class_id_of_object(this);
    // A reference array reports its *component* class id, so `Foo[]` would
    // otherwise compare class-equal to a `Foo` record and have its payload
    // read as record components. A record is final, so only a plain object of
    // the identical class can be equal.
    if this_class != ctx.class_id_of_object(other)
        || ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object
    {
        return Ok(Some(false));
    }
    let (_, first_slot, num_components) = object_method_fast_path_of(ctx, this_class);

    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let mut equal: Result<Option<bool>, MethodCallFailed> = Ok(Some(true));
    // LAST component first (interpreter round i1 wave 23, lane L4).
    // `ObjectMethods.makeEquals` folds the components into nested
    // `guardWithTest` handles, each new one outermost, so HotSpot runs the
    // last component's comparison first and stops at the first difference.
    // A component whose `equals` has a side effect or throws makes the order
    // visible: `record P(C a, C b)` with both `equals` throwing raises `b`'s
    // exception on HotSpot (probe `tools/probes/interp/L4/L4W23RecordObjectMethods.java`).
    for index in (first_slot..first_slot + num_components).rev() {
        let this_current = ctx.read_native_pin(this_pin, this);
        let other_current = ctx.read_native_pin(other_pin, other);
        let left = ctx.get_field(this_current, index);
        let right = ctx.get_field(other_current, index);
        match components_equal(ctx, &left, &right, depth, calls) {
            Ok(Some(true)) => continue,
            Ok(answer) => {
                // `Some(false)`: the first difference ends it. `None`
                // (`Calls::Refuse`): every component compared so far was
                // frame-free and equal, so nothing observable has run.
                equal = Ok(answer);
                break;
            }
            Err(failure) => {
                equal = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(other_pin);
    ctx.unpin_native_roots(this_pin);
    equal
}

/// Hash one record component the way the JDK's generated body does:
/// primitives by their wrapper hash, references through `Objects.hashCode`
/// (`null` → `0`). A reference with a frame-free shape is answered natively;
/// any other is called through `Objects.hashCode` or refused, per `calls`.
fn component_hash(
    ctx: &mut dyn NativeContext,
    component: &Value,
    depth: u32,
    calls: Calls,
) -> Result<Option<i32>, MethodCallFailed> {
    let Value::Object(Some(obj)) = *component else {
        return Ok(Some(primitive_hash(component)));
    };
    // The frame-free answer may run a boxed component's `hashCode` (Java, so
    // a collection); the call below needs the current reference.
    let pin = ctx.pin_native_root(obj);
    let without_frames = reference_hash_without_frames(ctx, obj, depth);
    let current = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    match without_frames? {
        Some(hash) => Ok(Some(hash)),
        None if calls == Calls::ThroughObjects => objects_hash_code(ctx, current).map(Some),
        None => Ok(None),
    }
}

/// Compare one record component the way the JDK's generated body does
/// (`Objects.equals(a, b)` for references). As [`component_hash`].
fn components_equal(
    ctx: &mut dyn NativeContext,
    left: &Value,
    right: &Value,
    depth: u32,
    calls: Calls,
) -> Result<Option<bool>, MethodCallFailed> {
    let (x, y) = match (left, right) {
        (Value::Object(Some(x)), Value::Object(Some(y))) => (*x, Some(*y)),
        // `Objects.equals(a, null)` is `a.equals(null)`, not `false`
        // (interpreter round i1 wave 39, lane L4): a class may answer `true`,
        // count the call or throw.
        (Value::Object(Some(x)), Value::Object(None)) => (*x, None),
        // `Objects.equals(null, b)` is `b == null`: no call.
        _ => return Ok(Some(primitives_equal(left, right))),
    };
    if Some(x) == y {
        return Ok(Some(true));
    }
    let x_pin = ctx.pin_native_root(x);
    let y_pin = y.map(|y| ctx.pin_native_root(y));
    let without_frames = references_equal_without_frames(ctx, x, y, depth);
    let x_now = ctx.read_native_pin(x_pin, x);
    let y_now = match (y, y_pin) {
        (Some(y), Some(pin)) => Some(ctx.read_native_pin(pin, y)),
        _ => None,
    };
    if let Some(pin) = y_pin {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(x_pin);
    match without_frames? {
        Some(equal) => Ok(Some(equal)),
        None if calls == Calls::ThroughObjects => objects_equals(ctx, x_now, y_now).map(Some),
        None => Ok(None),
    }
}

/// `Objects.hashCode(obj)`, CALLED: the frame HotSpot's handle chain shows
/// below a component's `hashCode`.
fn objects_hash_code(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Result<i32, MethodCallFailed> {
    match ctx.invoke_static(
        "java/util/Objects",
        "hashCode",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(obj))],
    )? {
        Some(Value::Int(hash)) => Ok(hash),
        _ => Ok(0),
    }
}

/// `Objects.equals(x, y)`, CALLED: the frame HotSpot's handle chain shows
/// below a component's `equals`.
fn objects_equals(
    ctx: &mut dyn NativeContext,
    x: ObjectRef,
    y: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    match ctx.invoke_static(
        "java/util/Objects",
        "equals",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[Value::Object(Some(x)), Value::Object(y)],
    )? {
        Some(Value::Int(result)) => Ok(result != 0),
        _ => Ok(false),
    }
}

/// `obj.hashCode()` when it can be answered with no Java frame a throw or a
/// stack walk could see, else `Ok(None)`:
///
/// * an array — `Object.hashCode`, the identity hash (a native);
/// * a `String` — the VM's compact string reader;
/// * a boxed primitive — its `value` ([`boxed_hash`]), or its own final
///   `hashCode`, called: a JDK method that neither throws nor looks at the
///   stack;
/// * a record with the generated body — this module, recursively, frame-free
///   all the way down;
/// * exactly `ArrayList` — its backing array, every element frame-free;
/// * an enum — the identity hash (`Enum.hashCode` is final).
fn reference_hash_without_frames(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    depth: u32,
) -> Result<Option<i32>, MethodCallFailed> {
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return Ok(Some(ctx.identity_hash_code(obj)));
    }
    // ONE classification per component. Probing `java_string_hash_code` first
    // instead cost a second class-manager lock on every NON-String component,
    // and this path runs tens of times per record hash.
    let (fast_path, slot) = component_fast_path_and_slot(ctx, obj);
    if fast_path & IS_STRING != 0 {
        // No Java dispatch, no host allocation.
        if let Some(hash) = ctx.java_string_hash_code(obj) {
            return Ok(Some(hash));
        }
    }
    if fast_path & BOXED_PRIMITIVE != 0 {
        // A boxed primitive (`Long id`, `Integer shard`): its `value`, no Java
        // dispatch (`CRATONVM_RECORD_BOXED_COMPONENTS`, default ON since i7
        // wave 144); otherwise its final `hashCode`, frame-free.
        if boxed_components_enabled() {
            if let Some(hash) = boxed_hash(ctx, obj, fast_path, slot) {
                return Ok(Some(hash));
            }
        }
        return match ctx.invoke_virtual(obj, "hashCode", "()I", &[])? {
            Some(Value::Int(hash)) => Ok(Some(hash)),
            _ => Ok(Some(0)),
        };
    }
    if fast_path & GENERATED_HASH_CODE != 0 && depth < MAX_NESTING {
        return record_hash_code_at(ctx, obj, depth + 1, Calls::Refuse);
    }
    if fast_path & IS_ARRAY_LIST != 0 && depth < MAX_NESTING {
        if let Some(hash) = array_list_hash(ctx, obj, depth)? {
            return Ok(Some(hash));
        }
    }
    if fast_path & IDENTITY_SEMANTICS != 0 {
        // Enum component: `Enum.hashCode` is final and returns the identity
        // hash — the same value `native_object_hash_code` produces for a
        // receiver that is not one of its three synthetic reflection types
        // (an enum never is), without that function's class-name lookup.
        return Ok(Some(ctx.identity_hash_code(obj)));
    }
    Ok(None)
}

/// `x.equals(y)` (`y` may be null) when it can be answered with no Java
/// frame a throw or a stack walk could see, else `Ok(None)`. The shapes of
/// [`reference_hash_without_frames`]; `x != y` already.
fn references_equal_without_frames(
    ctx: &mut dyn NativeContext,
    x: ObjectRef,
    y: Option<ObjectRef>,
    depth: u32,
) -> Result<Option<bool>, MethodCallFailed> {
    if ctx.heap_kind_of(x) == cratonvm_types::ObjectKind::Array {
        // `Object.equals`: identity, and `x != y`.
        return Ok(Some(false));
    }
    let (fast_path, slot) = component_fast_path_and_slot(ctx, x);
    let Some(y) = y else {
        // Only the shapes whose `equals(null)` is provably `false` skip the
        // call: a String, a record with the generated body
        // (`Class.isInstance(null)`), an enum (identity), exactly `ArrayList`
        // (`null instanceof List`) and a boxed primitive (`instanceof`).
        let proven_false =
            IS_STRING | GENERATED_EQUALS | IDENTITY_SEMANTICS | IS_ARRAY_LIST | BOXED_PRIMITIVE;
        return Ok((fast_path & proven_false != 0).then_some(false));
    };
    if fast_path & BOXED_PRIMITIVE != 0 {
        if boxed_components_enabled() {
            if let Some(equal) = boxed_equals(ctx, x, y, slot) {
                return Ok(Some(equal));
            }
        }
        return match ctx.invoke_virtual(
            x,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(y))],
        )? {
            Some(Value::Int(result)) => Ok(Some(result != 0)),
            _ => Ok(Some(false)),
        };
    }
    if fast_path & IS_STRING != 0 {
        // `java_strings_equal` reports `None` when either side is not a String,
        // in which case content equality is the wrong answer and the general
        // path must run.
        if let Some(equal) = ctx.java_strings_equal(x, y) {
            return Ok(Some(equal));
        }
    }
    if fast_path & GENERATED_EQUALS != 0 && depth < MAX_NESTING {
        // A record's `equals` requires the same class, so `y`'s class is
        // re-checked inside `record_equals_at`.
        return record_equals_at(ctx, x, y, depth + 1, Calls::Refuse);
    }
    if fast_path & IS_ARRAY_LIST != 0 && depth < MAX_NESTING {
        // `ArrayList.equals` accepts any `List`, but the native walk needs
        // both layouts, so require both sides to be plain ArrayLists.
        if component_fast_path(ctx, y) & IS_ARRAY_LIST != 0 {
            if let Some(equal) = array_lists_equal(ctx, x, y, depth)? {
                return Ok(Some(equal));
            }
        }
    }
    if fast_path & IDENTITY_SEMANTICS != 0 {
        // Enum component: `Enum.equals` is final reference equality, and the
        // `x == y` check above already failed.
        return Ok(Some(false));
    }
    Ok(None)
}

/// Read an `ArrayList`'s `(elementData, size)`, or `None` if the layout cannot
/// be resolved or is inconsistent — in which case the caller must fall back to
/// the Java dispatch rather than guess.
fn array_list_backing(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
) -> Option<(Option<ObjectRef>, usize)> {
    let data_slot = ctx.resolve_field_index("java/util/ArrayList", "elementData")?;
    let size_slot = ctx.resolve_field_index("java/util/ArrayList", "size")?;
    let Value::Int(size) = ctx.get_field(list, size_slot) else {
        return None;
    };
    let size = usize::try_from(size).ok()?;
    match ctx.get_field(list, data_slot) {
        // An empty list may legitimately share the shared-empty array, and a
        // `null` backing array with size 0 is also consistent.
        Value::Object(None) if size == 0 => Some((None, 0)),
        Value::Object(Some(data)) => {
            // Reject a size that does not fit the array rather than reading
            // out of bounds: something about the layout is not what we think.
            if ctx.array_length(data) < size {
                return None;
            }
            Some((Some(data), size))
        }
        _ => None,
    }
}

/// `java.util.List.hashCode()` over an `ArrayList`, computed natively.
///
/// The contract is fixed by the interface: `h = 1`, then
/// `h = 31 * h + Objects.hashCode(element)` in iteration order. Elements are
/// hashed frame-free ([`component_hash`] under [`Calls::Refuse`]), so
/// `String`, enum and nested-record elements need no Java dispatch either —
/// which is the whole point, since a Hibernate `FlushOperationGroup` carries
/// one of these per operation group.
///
/// `Ok(None)` means "layout not recognised, or an element needs a Java call:
/// use the Java path" (`ArrayList.hashCode` itself, whose frames a throwing
/// element must see).
fn array_list_hash(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    depth: u32,
) -> Result<Option<i32>, MethodCallFailed> {
    let Some((data, size)) = array_list_backing(ctx, list) else {
        return Ok(None);
    };
    let Some(data) = data else {
        // Empty list: `List.of().hashCode()` is 1.
        return Ok(Some(1));
    };
    // Hashing an element can call back into Java and therefore move the heap;
    // pin the backing array and re-read it per element.
    let pin = ctx.pin_native_root(data);
    let mut hash: Result<Option<i32>, MethodCallFailed> = Ok(Some(1));
    for index in 0..size {
        let current = ctx.read_native_pin(pin, data);
        let element = ctx.get_array_element(current, index);
        match component_hash(ctx, &element, depth + 1, Calls::Refuse) {
            Ok(Some(element_hash)) => {
                hash = Ok(Some(
                    hash.ok()
                        .flatten()
                        .unwrap_or(1)
                        .wrapping_mul(31)
                        .wrapping_add(element_hash),
                ));
            }
            Ok(None) => {
                hash = Ok(None);
                break;
            }
            Err(failure) => {
                hash = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    hash
}

/// `java.util.List.equals()` for two `ArrayList`s, computed natively: equal
/// sizes, then element-wise `Objects.equals` in order, every element
/// frame-free.
///
/// `Ok(None)` means "layout not recognised, or an element needs a Java call
/// before a difference was found: use the Java path".
fn array_lists_equal(
    ctx: &mut dyn NativeContext,
    left: ObjectRef,
    right: ObjectRef,
    depth: u32,
) -> Result<Option<bool>, MethodCallFailed> {
    let Some((left_data, left_size)) = array_list_backing(ctx, left) else {
        return Ok(None);
    };
    let Some((right_data, right_size)) = array_list_backing(ctx, right) else {
        return Ok(None);
    };
    if left_size != right_size {
        return Ok(Some(false));
    }
    let (Some(left_data), Some(right_data)) = (left_data, right_data) else {
        // Both empty (sizes are equal and at least one backing array is null).
        return Ok(Some(true));
    };
    let left_pin = ctx.pin_native_root(left_data);
    let right_pin = ctx.pin_native_root(right_data);
    let mut equal: Result<Option<bool>, MethodCallFailed> = Ok(Some(true));
    for index in 0..left_size {
        let left_current = ctx.read_native_pin(left_pin, left_data);
        let right_current = ctx.read_native_pin(right_pin, right_data);
        let a = ctx.get_array_element(left_current, index);
        let b = ctx.get_array_element(right_current, index);
        match components_equal(ctx, &a, &b, depth + 1, Calls::Refuse) {
            Ok(Some(true)) => continue,
            Ok(answer) => {
                equal = Ok(answer);
                break;
            }
            Err(failure) => {
                equal = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(right_pin);
    ctx.unpin_native_roots(left_pin);
    equal
}

/// Fast-path mask for a **reference component**, or `0` when the component
/// must go through `invoke_virtual`.
///
/// Arrays are always `0`: a reference array carries its *component* class id,
/// so `String[]` reports `java/lang/String` and `Foo[]` reports the record
/// `Foo` — treating either as an instance of that class would read the array's
/// payload as record components. `Object.hashCode`/`equals` (identity) is the
/// correct behaviour for arrays and `invoke_virtual` already delivers it.
fn component_fast_path(ctx: &mut dyn NativeContext, obj: ObjectRef) -> u8 {
    component_fast_path_and_slot(ctx, obj).0
}

/// [`component_fast_path`] with the answer's `.1`: for a boxed primitive
/// ([`BOXED_PRIMITIVE`]) the slot of its `value` field.
fn component_fast_path_and_slot(ctx: &mut dyn NativeContext, obj: ObjectRef) -> (u8, usize) {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return (0, 0);
    }
    let class_id = ctx.class_id_of_object(obj);
    let (bits, slot, _) = object_method_fast_path_of(ctx, class_id);
    (bits, slot)
}

/// `CRATONVM_RECORD_BOXED_COMPONENTS` (default ON since i7 wave 144, `=0` off, performance; interpreter
/// round i7 wave 135, lane L3, `i134-R2-record-layout-lookup-is-35x-hotspot`).
/// Read through the per-VM flag snapshot.
fn boxed_components_enabled() -> bool {
    crate::nbflags().record_boxed_components
}

/// `Objects.hashCode` of a boxed-primitive component (`fast` carries
/// [`BOXED_PRIMITIVE`]), answered from its `value` field at `slot`, or `None`
/// when the field does not read back as the wrapper's primitive (then the
/// caller dispatches `hashCode` as before).
///
/// Exact: the eight wrappers are final and their `hashCode` is the JDK's
/// fixed wrapper hash of `value` -- the value itself for `Integer`, `Short`,
/// `Byte` and `Character`, `(int)(v ^ (v >>> 32))` for `Long`,
/// `floatToIntBits` / `doubleToLongBits` for `Float` / `Double` (NaN
/// canonical), 1231/1237 for `Boolean` -- which is what [`primitive_hash`]
/// computes for a component of that primitive type. What the dispatch did
/// and this does not: run the wrapper's `hashCode` bytecode, which only an
/// agent that retransforms `java.lang.Long` could tell (the `String` fast
/// path above has the same property).
fn boxed_hash(ctx: &mut dyn NativeContext, obj: ObjectRef, fast: u8, slot: usize) -> Option<i32> {
    match ctx.get_field(obj, slot) {
        Value::Int(v) if fast & BOXED_BOOLEAN != 0 => Some(if v != 0 {
            BOOLEAN_HASH_TRUE
        } else {
            BOOLEAN_HASH_FALSE
        }),
        _ if fast & BOXED_BOOLEAN != 0 => None,
        value @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) => {
            Some(primitive_hash(&value))
        }
        _ => None,
    }
}

/// `x.equals(y)` for a boxed-primitive `x` (`x_fast` carries
/// [`BOXED_PRIMITIVE`], `x_slot` its `value` slot) and a non-null `y` that is
/// not `x`: `false` unless `y` is an object of exactly `x`'s class (the
/// wrappers are final, so `instanceof` is class identity), then the two
/// `value`s compared as [`primitives_equal`] compares components (`Float` /
/// `Double` by `floatToIntBits` / `doubleToLongBits`, as their `equals`).
/// `None` when a field does not read back as a primitive (the caller
/// dispatches `equals` as before).
fn boxed_equals(
    ctx: &mut dyn NativeContext,
    x: ObjectRef,
    y: ObjectRef,
    x_slot: usize,
) -> Option<bool> {
    if ctx.heap_kind_of(y) != cratonvm_types::ObjectKind::Object
        || ctx.class_id_of_object(y) != ctx.class_id_of_object(x)
    {
        return Some(false);
    }
    let left = ctx.get_field(x, x_slot);
    let right = ctx.get_field(y, x_slot);
    match (&left, &right) {
        (Value::Int(_), Value::Int(_))
        | (Value::Long(_), Value::Long(_))
        | (Value::Float(_), Value::Float(_))
        | (Value::Double(_), Value::Double(_)) => Some(primitives_equal(&left, &right)),
        _ => None,
    }
}

/// Which components of a record class are declared `boolean`, memoised.
///
/// G9-1. `Value::Int` is the JVM's carrier for `boolean`, `byte`, `char`,
/// `short` and `int`, and four of those five hash to the value itself. The
/// fifth does not: measured on OpenJDK 25.0.3+9, `record Z(boolean b) {}` gives
/// `new Z(false).hashCode() == 1237` and `new Z(true).hashCode() == 1231`,
/// because the generated body calls `Boolean.hashCode`. The direct
/// implementation answered `0` and `1`.
///
/// The declared type is not on the component value, so it has to come from the
/// class — and `NativeContext::record_components` is far too expensive to call
/// per hash: it takes the class-manager read lock and clones a `String` pair
/// per component. Hence the memo, and hence the guard at the ONE call site:
/// it is consulted only when a component reads back as `Int(0)` or `Int(1)`,
/// the only two values at which `Boolean.hashCode` and `Integer.hashCode` can
/// possibly disagree. A `boolean` field cannot hold anything else.
///
/// Per VM (interpreter round i6 wave 117, lane L4; page
/// `i39-L4-proposal-one-record-layout-table-per-class`): a `ClassId` is an
/// index into ONE VM's class manager, so the memo lives in THAT VM's native
/// registry (`NativeContext::record_boolean_components_slot`), keyed by the
/// class id alone, and dies with the VM. It was a process-wide `static` keyed
/// by `(vm_identity, ClassId)` with a forget hook (wave 39), which AGENTS.md
/// rules out. Class ids are never reused within a VM, so no row goes stale.
/// A context without the slot (the mocks) reads the components per call.
/// Read-mostly after warmup.
///
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. The read copies a `bool`
/// out and the write inserts an already-built `Box<[bool]>`; the one
/// `NativeContext` call in [`is_boolean_component`] (`record_components`)
/// runs BETWEEN them, with no guard held.
struct BooleanComponents {
    flags: cratonvm_types::lock_order::OrderedPlRwLock<
        std::collections::HashMap<u32, RecordLayoutRow>,
    >,
}

/// One class's row of the per-VM record-layout table (interpreter round i6
/// wave 122, lane L4; `i39-L4-proposal-one-record-layout-table-per-class`,
/// items 1-2). The `boolean` flags are the wave-117 memo, filed lazily as
/// before; `fast` is `NativeContext::object_method_fast_path`'s answer for the
/// class (generated-body bits, first component slot, component count), filed
/// only under `CRATONVM_RECORD_LAYOUT_TABLE` so a record hash no longer takes
/// the class-manager lock per record (and per nested reference component).
///
/// Every part is a fact about the CLASS that cannot change for the life of
/// its id: the component list and `first_field_index` are fixed at
/// definition (a redefinition may not add or remove fields), the
/// generated-body bits are already memoised for good on the `Class`
/// (`Class::record_object_methods`, copied across a redefinition, never
/// reset), and the `String` / `ArrayList` / enum bits are by name and
/// superclass. Class ids are never reused within a VM.
#[derive(Default)]
struct RecordLayoutRow {
    booleans: Option<Box<[bool]>>,
    fast: Option<(u8, usize, usize)>,
}

impl BooleanComponents {
    fn new() -> Self {
        Self {
            flags: cratonvm_types::lock_order::OrderedPlRwLock::new(
                std::collections::HashMap::new(),
                cratonvm_types::lock_order::LockLevel::Scratch,
            ),
        }
    }

    /// The filed answer for component `index` of class `class`, if the class
    /// is filed (a component past its count is not `boolean`).
    fn lookup(&self, class: u32, index: usize) -> Option<bool> {
        let map = self.flags.read();
        map.get(&class)
            .and_then(|row| row.booleans.as_ref())
            .map(|flags| flags.get(index).copied().unwrap_or(false))
    }

    /// File class `class`'s per-component `boolean` flags.
    fn file(&self, class: u32, flags: Box<[bool]>) {
        self.flags.write().entry(class).or_default().booleans = Some(flags);
    }

    /// The filed `object_method_fast_path` answer for class `class`.
    fn fast_path(&self, class: u32) -> Option<(u8, usize, usize)> {
        self.flags.read().get(&class).and_then(|row| row.fast)
    }

    /// File class `class`'s `object_method_fast_path` answer.
    fn file_fast_path(&self, class: u32, fast: (u8, usize, usize)) {
        self.flags.write().entry(class).or_default().fast = Some(fast);
    }
}

/// `NativeContext::object_method_fast_path` for `class_id`, through this VM's
/// record-layout table under `CRATONVM_RECORD_LAYOUT_TABLE` (default off,
/// performance; see [`RecordLayoutRow`]): the first call per class asks the
/// context (the class-manager read) and files the answer, later ones read the
/// row. Off, or a context without the per-VM slot (the mocks): the context's
/// answer per call, as before.
fn object_method_fast_path_of(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> (u8, usize, usize) {
    if !crate::nbflags().record_layout_table {
        return ctx.object_method_fast_path(class_id);
    }
    let class = class_id.as_u32();
    if let Some(hit) = boolean_components(&*ctx).and_then(|m| m.fast_path(class)) {
        return hit;
    }
    let answer = ctx.object_method_fast_path(class_id);
    // Filed whatever it is, `(0, 0, 0)` included: that is the common answer
    // for a nested component of an ordinary class (`Long`, a user bean), the
    // very read this table saves. The id comes from a live object
    // (`class_id_of_object`), so the class is defined and its answer final.
    if let Some(memo) = boolean_components(&*ctx) {
        memo.file_fast_path(class, answer);
    }
    answer
}

/// THIS VM's memo, created at the first call; `None` for a context with no
/// per-VM slot (the mocks).
fn boolean_components(ctx: &dyn NativeContext) -> Option<&BooleanComponents> {
    let slot = ctx.record_boolean_components_slot()?;
    let boxed = slot.get_or_init(|| -> Box<dyn std::any::Any + Send + Sync> {
        Box::new(BooleanComponents::new())
    });
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<BooleanComponents>()
}

/// `true` if component `component_index` of `class_id` is declared `boolean`.
fn is_boolean_component(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    component_index: usize,
) -> bool {
    let class = class_id.as_u32();
    if let Some(hit) = boolean_components(&*ctx).and_then(|m| m.lookup(class, component_index)) {
        return hit;
    }
    let flags: Box<[bool]> = ctx
        .record_components(class_id)
        .iter()
        .map(|(_, descriptor)| descriptor == "Z")
        .collect();
    let answer = flags.get(component_index).copied().unwrap_or(false);
    if let Some(memo) = boolean_components(&*ctx) {
        memo.file(class, flags);
    }
    answer
}

/// `Boolean.hashCode` — the two constants the JDK has used since 1.0.
const BOOLEAN_HASH_TRUE: i32 = 1231;
/// See [`BOOLEAN_HASH_TRUE`].
const BOOLEAN_HASH_FALSE: i32 = 1237;

/// `Float.floatToIntBits` — NOT `floatToRawIntBits`.
///
/// G9-1, measured on OpenJDK 25.0.3+9. The generated record body hashes a
/// `float` component with `Float.hashCode(f)`, which is `floatToIntBits`, and
/// that COLLAPSES every NaN to the canonical `0x7FC00000`. `f32::to_bits` is
/// `floatToRawIntBits` and keeps the payload:
///
/// ```text
///   record F(float v) {}                       HotSpot      was
///   new F(Float.intBitsToFloat(0x7F800001))
///       .hashCode()                            2143289344   2139095041
/// ```
///
/// Reachable from ordinary bytecode — `Float.intBitsToFloat` is the way in,
/// and a signalling NaN out of native code is another. Every non-NaN input,
/// including both zeros and both infinities, is unaffected: the two functions
/// differ on NaN and nowhere else.
#[inline]
fn float_to_int_bits(f: f32) -> i32 {
    if f.is_nan() {
        0x7FC0_0000u32 as i32
    } else {
        f.to_bits() as i32
    }
}

/// [`float_to_int_bits`]'s twin — `Double.doubleToLongBits`. Canonical NaN is
/// `0x7FF8000000000000`. Measured: `new D(Double.longBitsToDouble(
/// 0x7FF0000000000001L)).hashCode()` is `2146959360` on HotSpot.
#[inline]
fn double_to_long_bits(d: f64) -> u64 {
    if d.is_nan() {
        0x7FF8_0000_0000_0000u64
    } else {
        d.to_bits()
    }
}

/// `Objects.hashCode` for a non-reference (or null) component value.
///
/// **This function cannot see a `boolean`.** `Value::Int` carries `boolean`,
/// `byte`, `char`, `short` and `int` alike, and the wrapper hash agrees for
/// four of the five: `Byte`/`Short`/`Character`/`Integer.hashCode` are all the
/// value itself, while `Boolean.hashCode` is `1231`/`1237`. So the answer is
/// wrong exactly when the declared component type is `Z` — see
/// [`boolean_component_mask`], which supplies the missing bit, and which is
/// consulted only for the two `int` values where it can possibly matter.
fn primitive_hash(value: &Value) -> i32 {
    match value {
        Value::Int(n) => *n,
        Value::Long(n) => (*n ^ (*n >> 32)) as i32,
        Value::Float(f) => float_to_int_bits(*f),
        Value::Double(d) => {
            let bits = double_to_long_bits(*d);
            (bits ^ (bits >> 32)) as i32
        }
        // `Objects.hashCode(null)` is 0.
        _ => 0,
    }
}

/// Per-component equality for non-reference (or null) values.
fn primitives_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        // The generated body compares a `float`/`double` component with
        // `Float.compare(a, b) == 0` / `Double.compare(a, b) == 0`, which goes
        // through `floatToIntBits` — so NaN equals NaN (including two NaNs
        // with DIFFERENT payloads) and `+0.0` does not equal `-0.0`.
        //
        // G9-1: the raw-bit form this replaced got the second rule right and
        // the first one only half right. Measured on OpenJDK 25.0.3+9 with
        // `record F(float v) {}`:
        //
        // ```text
        //   F(0.0f).equals(F(-0.0f))                       false   false  ok
        //   F(NaN).equals(F(NaN))                          true    true   ok
        //   F(intBitsToFloat(0x7F800001)).equals(F(NaN))    true   FALSE  wrong
        // ```
        (Value::Float(x), Value::Float(y)) => float_to_int_bits(*x) == float_to_int_bits(*y),
        (Value::Double(x), Value::Double(y)) => double_to_long_bits(*x) == double_to_long_bits(*y),
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    use super::*;
    use cratonvm_native_api::NativeCallback;

    #[test]
    fn handlers_match_native_callback_signature() {
        let hash_code: NativeCallback = intrinsic_record_hash_code;
        let equals: NativeCallback = intrinsic_record_equals;
        assert_ne!(hash_code as usize, equals as usize);
    }

    #[test]
    fn primitive_hash_matches_wrapper_semantics() {
        assert_eq!(primitive_hash(&Value::Int(7)), 7);
        assert_eq!(primitive_hash(&Value::Long(1)), 1);
        // Long.hashCode(0x1_0000_0000) == (high ^ low) == 1
        assert_eq!(primitive_hash(&Value::Long(1 << 32)), 1);
        assert_eq!(primitive_hash(&Value::Object(None)), 0);
        assert_eq!(
            primitive_hash(&Value::Float(1.5f32)),
            1.5f32.to_bits() as i32
        );
    }

    #[test]
    fn primitives_equal_uses_bit_semantics_for_floats() {
        assert!(primitives_equal(
            &Value::Double(f64::NAN),
            &Value::Double(f64::NAN)
        ));
        assert!(!primitives_equal(&Value::Double(0.0), &Value::Double(-0.0)));
        assert!(primitives_equal(&Value::Object(None), &Value::Object(None)));
        assert!(!primitives_equal(&Value::Int(1), &Value::Long(1)));
    }

    /// Every expectation below is the verbatim OpenJDK 25.0.3+9 answer for the
    /// javac-generated body, captured by the `RecProbe` differential (G9-1).
    ///
    /// The discriminating input is a NaN with a NON-canonical payload, which is
    /// the only place `floatToIntBits` and `floatToRawIntBits` differ. Reached
    /// from ordinary bytecode through `Float.intBitsToFloat`.
    #[test]
    fn a_nan_component_hashes_and_compares_by_canonical_bits() {
        // record F(float v) {}  new F(intBitsToFloat(0x7F800001)).hashCode()
        let odd_nan = f32::from_bits(0x7F80_0001);
        assert!(odd_nan.is_nan());
        assert_eq!(primitive_hash(&Value::Float(odd_nan)), 2_143_289_344);
        assert_eq!(primitive_hash(&Value::Float(f32::NAN)), 2_143_289_344);
        // record D(double v) {}  new D(longBitsToDouble(0x7FF0000000000001L))
        let odd_dnan = f64::from_bits(0x7FF0_0000_0000_0001);
        assert!(odd_dnan.is_nan());
        assert_eq!(primitive_hash(&Value::Double(odd_dnan)), 2_146_959_360);
        assert_eq!(primitive_hash(&Value::Double(f64::NAN)), 2_146_959_360);

        // Two NaNs with different payloads ARE equal components on HotSpot.
        assert!(primitives_equal(
            &Value::Float(odd_nan),
            &Value::Float(f32::NAN)
        ));
        assert!(primitives_equal(
            &Value::Double(odd_dnan),
            &Value::Double(f64::NAN)
        ));

        // Controls: the non-NaN answers must not have moved. `-0.0` keeps its
        // sign bit in both the hash and the comparison.
        assert_eq!(primitive_hash(&Value::Float(1.5f32)), 1_069_547_520);
        assert_eq!(primitive_hash(&Value::Float(-0.0f32)), i32::MIN);
        assert_eq!(primitive_hash(&Value::Double(-0.0f64)), i32::MIN);
        assert!(!primitives_equal(
            &Value::Float(0.0f32),
            &Value::Float(-0.0f32)
        ));
        assert!(!primitives_equal(&Value::Double(0.0), &Value::Double(-0.0)));
    }

    /// `Boolean.hashCode`'s two constants, which is what a `boolean` record
    /// component hashes to — measured, `new Z(false).hashCode() == 1237`.
    ///
    /// The full path needs a live class (`is_boolean_component` reads the
    /// record's descriptors), which a leaf-crate unit test cannot build; what
    /// is pinned here is the constant pair and the guard's own arithmetic,
    /// i.e. that `primitive_hash` is still the INT answer for `0`/`1` so the
    /// two are genuinely different values and the branch is not a no-op.
    /// Interpreter round i6 wave 117, lane L4: the `boolean`-component memo is
    /// per VM. Two memos (two VMs) never answer each other's class ids, a
    /// filed class answers per component (past its count: not `boolean`), and
    /// an unfiled one is a miss.
    #[test]
    fn boolean_component_memo_is_per_vm() {
        let vm_a = BooleanComponents::new();
        let vm_b = BooleanComponents::new();
        vm_a.file(7, vec![false, true].into_boxed_slice());
        assert_eq!(vm_a.lookup(7, 0), Some(false));
        assert_eq!(vm_a.lookup(7, 1), Some(true));
        assert_eq!(vm_a.lookup(7, 2), Some(false));
        assert_eq!(vm_a.lookup(8, 0), None);
        assert_eq!(vm_b.lookup(7, 1), None, "another VM's class 7 is not this one's");
        vm_b.file(7, vec![true].into_boxed_slice());
        assert_eq!(vm_b.lookup(7, 0), Some(true));
        assert_eq!(vm_a.lookup(7, 0), Some(false));
    }

    /// Interpreter round i6 wave 122, lane L4 (`i39-L4`, items 1-2): a class's
    /// row holds the `boolean` flags and the fast-path answer apart. Filing one
    /// half leaves the other a miss, `(0, 0, 0)` is a real filed answer, and
    /// two VMs' tables stay apart.
    #[test]
    fn record_layout_rows_file_their_halves_apart() {
        let vm_a = BooleanComponents::new();
        let vm_b = BooleanComponents::new();
        vm_a.file_fast_path(7, (1 | 2, 0, 2));
        assert_eq!(vm_a.fast_path(7), Some((3, 0, 2)));
        assert_eq!(vm_a.lookup(7, 0), None, "the boolean half is not filed yet");
        vm_a.file(7, vec![true, false].into_boxed_slice());
        assert_eq!(vm_a.lookup(7, 0), Some(true));
        assert_eq!(vm_a.fast_path(7), Some((3, 0, 2)), "filing booleans keeps the fast half");
        vm_a.file_fast_path(9, (0, 0, 0));
        assert_eq!(vm_a.fast_path(9), Some((0, 0, 0)));
        assert_eq!(vm_b.fast_path(7), None);
    }

    /// A context with no per-VM slot (the mock) has no memo: the components
    /// are read per call, and nothing is filed anywhere.
    #[test]
    fn a_context_without_the_slot_reads_per_call() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        assert!(boolean_components(&ctx).is_none());
        assert!(!is_boolean_component(
            &mut ctx,
            cratonvm_types::ClassId::new(3),
            0
        ));
        assert!(boolean_components(&ctx).is_none());
    }

    /// Interpreter round i7 wave 135, lane L3 (`i134-R2`,
    /// `CRATONVM_RECORD_BOXED_COMPONENTS`): a boxed primitive's hash and
    /// `equals` from its `value` slot are the wrapper's (`Long.hashCode`,
    /// `Boolean.hashCode`, `Double.equals` by `doubleToLongBits`), a different
    /// class is never equal, and a slot that does not read back as the
    /// primitive declines (`None`: the caller dispatches as before).
    #[test]
    fn i135_l3_boxed_components_answer_as_the_wrappers() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let long_class = cratonvm_types::ClassId::new(41);
        let int_class = cratonvm_types::ClassId::new(42);
        let bool_class = cratonvm_types::ClassId::new(43);
        let double_class = cratonvm_types::ClassId::new(44);
        let boxed = |ctx: &mut crate::test_utils::MockNativeContext, class, value| {
            let obj = ctx.alloc_object(class, 1);
            ctx.set_field(obj, 0, value);
            obj
        };
        // `Long.valueOf(0x1_0000_0002L).hashCode()` is `1 ^ 2 == 3`.
        let l1 = boxed(&mut ctx, long_class, Value::Long(0x1_0000_0002));
        assert_eq!(boxed_hash(&mut ctx, l1, BOXED_PRIMITIVE, 0), Some(3));
        let l2 = boxed(&mut ctx, long_class, Value::Long(0x1_0000_0002));
        let i2 = boxed(&mut ctx, int_class, Value::Int(2));
        assert_eq!(boxed_equals(&mut ctx, l1, l2, 0), Some(true));
        assert_eq!(
            boxed_equals(&mut ctx, l1, i2, 0),
            Some(false),
            "Long.equals(Integer) is false"
        );
        let t = boxed(&mut ctx, bool_class, Value::Int(1));
        let f = boxed(&mut ctx, bool_class, Value::Int(0));
        let boolean = BOXED_PRIMITIVE | BOXED_BOOLEAN;
        assert_eq!(boxed_hash(&mut ctx, t, boolean, 0), Some(BOOLEAN_HASH_TRUE));
        assert_eq!(boxed_hash(&mut ctx, f, boolean, 0), Some(BOOLEAN_HASH_FALSE));
        assert_eq!(boxed_equals(&mut ctx, t, f, 0), Some(false));
        // `Double.valueOf(odd NaN).equals(Double.NaN)` is true; `0.0` vs `-0.0` false.
        let odd_nan = Value::Double(f64::from_bits(0x7FF0_0000_0000_0001));
        let n1 = boxed(&mut ctx, double_class, odd_nan);
        let n2 = boxed(&mut ctx, double_class, Value::Double(f64::NAN));
        assert_eq!(boxed_equals(&mut ctx, n1, n2, 0), Some(true));
        assert_eq!(boxed_hash(&mut ctx, n1, BOXED_PRIMITIVE, 0), Some(2_146_959_360));
        let z1 = boxed(&mut ctx, double_class, Value::Double(0.0));
        let z2 = boxed(&mut ctx, double_class, Value::Double(-0.0));
        assert_eq!(boxed_equals(&mut ctx, z1, z2, 0), Some(false));
        // A slot that is not a primitive declines.
        let odd = boxed(&mut ctx, long_class, Value::Object(None));
        assert_eq!(boxed_hash(&mut ctx, odd, BOXED_PRIMITIVE, 0), None);
        assert_eq!(boxed_hash(&mut ctx, l1, boolean, 0), None, "a Boolean slot must be an int");
        assert_eq!(boxed_equals(&mut ctx, odd, odd, 0), None);
    }

    #[test]
    fn boolean_component_hash_constants() {
        assert_eq!(BOOLEAN_HASH_TRUE, 1231);
        assert_eq!(BOOLEAN_HASH_FALSE, 1237);
        assert_eq!(primitive_hash(&Value::Int(1)), 1);
        assert_eq!(primitive_hash(&Value::Int(0)), 0);
        assert_ne!(primitive_hash(&Value::Int(1)), BOOLEAN_HASH_TRUE);
        assert_ne!(primitive_hash(&Value::Int(0)), BOOLEAN_HASH_FALSE);
        // Every other int value is the same under both wrappers, which is why
        // the guard is `0 | 1` and not "every Int component".
        for v in [-1i32, 2, 7, i32::MIN, i32::MAX] {
            assert_eq!(primitive_hash(&Value::Int(v)), v);
        }
    }
}
