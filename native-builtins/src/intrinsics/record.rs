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
//! Anything else (other collections, ordinary classes) still goes through
//! `invoke_virtual`, exactly as before.
//!
//! Recursion depth is bounded by [`MAX_NESTING`]; past that the component
//! falls back to `invoke_virtual`, which cannot loop forever because a record
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

/// Native-recursion depth cap for nested record components.
const MAX_NESTING: u32 = 64;

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ObjectRef, Value};

/// Intrinsic for a record's generated `hashCode ()I` (virtual).
///
/// `args == [receiver]`.
pub fn intrinsic_record_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // A null receiver never reaches a virtual call site (the interpreter
        // raises the NPE first); mirror the invokedynamic executor's `0`.
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(record_hash_code(ctx, this)?)))
}

/// Intrinsic for a record's generated `equals (Ljava/lang/Object;)Z` (virtual).
///
/// `args == [receiver, other]`.
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
    let equal = record_equals(ctx, this, other)?;
    Ok(Some(Value::Int(i32::from(equal))))
}

/// `hashCode` for a record instance: `h = 0; h = h * 31 + hash(component)` in
/// component declaration order, matching what the JDK's `ObjectMethods`
/// bootstrap produces.
pub fn record_hash_code(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    record_hash_code_at(ctx, this, 0)
}

/// `equals` for a record instance: same class, then every component compared
/// with the JDK's per-component semantics (primitives by value with
/// `Float`/`Double` bit equality, references via `Objects.equals`).
pub fn record_equals(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    record_equals_at(ctx, this, other, 0)
}

fn record_hash_code_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    depth: u32,
) -> Result<i32, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(this);
    let (_, first_slot, num_components) = ctx.object_method_fast_path(class_id);

    // The component reads themselves cannot move the heap, but hashing a
    // reference component may call back into Java and therefore GC. Pin the
    // receiver and re-read it per component so a moved object is followed.
    let pin = ctx.pin_native_root(this);
    let mut hash: Result<i32, MethodCallFailed> = Ok(0);
    for index in first_slot..first_slot + num_components {
        let current = ctx.read_native_pin(pin, this);
        let component = ctx.get_field(current, index);
        match component_hash(ctx, &component, depth) {
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

fn record_equals_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
    depth: u32,
) -> Result<bool, MethodCallFailed> {
    if this == other {
        return Ok(true);
    }
    let this_class = ctx.class_id_of_object(this);
    // A reference array reports its *component* class id, so `Foo[]` would
    // otherwise compare class-equal to a `Foo` record and have its payload
    // read as record components. A record is final, so only a plain object of
    // the identical class can be equal.
    if this_class != ctx.class_id_of_object(other)
        || ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object
    {
        return Ok(false);
    }
    let (_, first_slot, num_components) = ctx.object_method_fast_path(this_class);

    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let mut equal: Result<bool, MethodCallFailed> = Ok(true);
    for index in first_slot..first_slot + num_components {
        let this_current = ctx.read_native_pin(this_pin, this);
        let other_current = ctx.read_native_pin(other_pin, other);
        let left = ctx.get_field(this_current, index);
        let right = ctx.get_field(other_current, index);
        match components_equal(ctx, &left, &right, depth) {
            Ok(true) => continue,
            Ok(false) => {
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

/// Hash one record component the way the JDK's generated body does:
/// primitives by their wrapper hash, references through their virtual
/// `hashCode` (`null` → `0`).
fn component_hash(
    ctx: &mut dyn NativeContext,
    component: &Value,
    depth: u32,
) -> Result<i32, MethodCallFailed> {
    let Value::Object(Some(obj)) = *component else {
        return Ok(primitive_hash(component));
    };
    // ONE classification per component. Probing `java_string_hash_code` first
    // instead cost a second class-manager lock on every NON-String component,
    // and this path runs tens of times per record hash.
    let fast_path = component_fast_path(ctx, obj);
    if fast_path & IS_STRING != 0 {
        // No Java dispatch, no host allocation.
        if let Some(hash) = ctx.java_string_hash_code(obj) {
            return Ok(hash);
        }
    }
    if fast_path & GENERATED_HASH_CODE != 0 && depth < MAX_NESTING {
        return record_hash_code_at(ctx, obj, depth + 1);
    }
    if fast_path & IS_ARRAY_LIST != 0 && depth < MAX_NESTING {
        if let Some(hash) = array_list_hash(ctx, obj, depth)? {
            return Ok(hash);
        }
    }
    if fast_path & IDENTITY_SEMANTICS != 0 {
        // Enum component: `Enum.hashCode` is final and returns the identity
        // hash — the same value `native_object_hash_code` produces for a
        // receiver that is not one of its three synthetic reflection types
        // (an enum never is), without that function's class-name lookup.
        return Ok(ctx.identity_hash_code(obj));
    }
    match ctx.invoke_virtual(obj, "hashCode", "()I", &[])? {
        Some(Value::Int(hash)) => Ok(hash),
        _ => Ok(0),
    }
}

/// Compare one record component the way the JDK's generated body does.
fn components_equal(
    ctx: &mut dyn NativeContext,
    left: &Value,
    right: &Value,
    depth: u32,
) -> Result<bool, MethodCallFailed> {
    let (Value::Object(Some(x)), Value::Object(Some(y))) = (left, right) else {
        return Ok(primitives_equal(left, right));
    };
    let (x, y) = (*x, *y);
    if x == y {
        return Ok(true);
    }
    let fast_path = component_fast_path(ctx, x);
    if fast_path & IS_STRING != 0 {
        // `java_strings_equal` reports `None` when either side is not a String,
        // in which case content equality is the wrong answer and the general
        // path must run.
        if let Some(equal) = ctx.java_strings_equal(x, y) {
            return Ok(equal);
        }
    }
    if fast_path & GENERATED_EQUALS != 0 && depth < MAX_NESTING {
        // A record's `equals` requires the same class, so `y`'s class is
        // re-checked inside `record_equals_at`.
        return record_equals_at(ctx, x, y, depth + 1);
    }
    if fast_path & IS_ARRAY_LIST != 0 && depth < MAX_NESTING {
        // `ArrayList.equals` accepts any `List`, but the native walk needs
        // both layouts, so require both sides to be plain ArrayLists.
        if component_fast_path(ctx, y) & IS_ARRAY_LIST != 0 {
            if let Some(equal) = array_lists_equal(ctx, x, y, depth)? {
                return Ok(equal);
            }
        }
    }
    if fast_path & IDENTITY_SEMANTICS != 0 {
        // Enum component: `Enum.equals` is final reference equality, and the
        // `x == y` check above already failed.
        return Ok(false);
    }
    match ctx.invoke_virtual(x, "equals", "(Ljava/lang/Object;)Z", &[Value::Object(Some(y))])? {
        Some(Value::Int(result)) => Ok(result != 0),
        _ => Ok(false),
    }
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
/// hashed by [`component_hash`], so `String`, enum and nested-record elements
/// need no Java dispatch either — which is the whole point, since a Hibernate
/// `FlushOperationGroup` carries one of these per operation group.
///
/// `Ok(None)` means "layout not recognised, use the Java path".
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
    let mut hash: Result<i32, MethodCallFailed> = Ok(1);
    for index in 0..size {
        let current = ctx.read_native_pin(pin, data);
        let element = ctx.get_array_element(current, index);
        match component_hash(ctx, &element, depth + 1) {
            Ok(element_hash) => {
                hash = Ok(hash.unwrap_or(1).wrapping_mul(31).wrapping_add(element_hash));
            }
            Err(failure) => {
                hash = Err(failure);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    hash.map(Some)
}

/// `java.util.List.equals()` for two `ArrayList`s, computed natively: equal
/// sizes, then element-wise `Objects.equals` in order.
///
/// `Ok(None)` means "layout not recognised, use the Java path".
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
    let mut equal: Result<bool, MethodCallFailed> = Ok(true);
    for index in 0..left_size {
        let left_current = ctx.read_native_pin(left_pin, left_data);
        let right_current = ctx.read_native_pin(right_pin, right_data);
        let a = ctx.get_array_element(left_current, index);
        let b = ctx.get_array_element(right_current, index);
        match components_equal(ctx, &a, &b, depth + 1) {
            Ok(true) => continue,
            Ok(false) => {
                equal = Ok(false);
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
    equal.map(Some)
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
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return 0;
    }
    ctx.object_method_fast_path(ctx.class_id_of_object(obj)).0
}

/// `Objects.hashCode` for a non-reference (or null) component value.
fn primitive_hash(value: &Value) -> i32 {
    match value {
        Value::Int(n) => *n,
        Value::Long(n) => (*n ^ (*n >> 32)) as i32,
        Value::Float(f) => f.to_bits() as i32,
        Value::Double(d) => {
            let bits = d.to_bits();
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
        // `Float.equals`/`Double.equals` bit semantics: NaN equals NaN and
        // +0.0 does not equal -0.0, which is what the generated record
        // `equals` uses for float/double components.
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
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
        assert!(primitives_equal(
            &Value::Object(None),
            &Value::Object(None)
        ));
        assert!(!primitives_equal(&Value::Int(1), &Value::Long(1)));
    }
}
