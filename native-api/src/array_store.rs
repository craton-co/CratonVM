// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The store check a `toArray(T[])`-shaped native owes, once.
//!
//! # The species this closes
//!
//! `Collection.toArray(T[])` is a NARROWING copy: the destination's component
//! type comes from the caller's template, so an element it cannot accept is an
//! `ArrayStoreException`. Every native that implements the method has to make
//! that check, and none of them did — compatible mode produced a `String[]`
//! holding an `Integer` (MEASURED, `probes/DodArrayStoreSweep`).
//!
//! `--dump-native-registry` says why this is a module and not a fix in one
//! file: **32 registrations of `toArray([Ljava/lang/Object;)` over 30 classes
//! from nine source sites**, in three crates. Seven private copies of one
//! twelve-line check is the shape `appended_slots` was written to collapse
//! fifteen of, and `instantiable` another.
//!
//! # Two routes, two messages, both measured
//!
//! The JDK throws from two different places depending on the receiver, and the
//! text differs. On HotSpot 25.0.4+7:
//!
//! ```text
//! ArrayList.toArray(new String[0])    ArrayStoreException: arraycopy: element type
//!                                     mismatch: can not cast one of the elements of
//!                                     java.lang.Object[] to the type of the
//!                                     destination array, java.lang.String
//! HashSet.toArray(new String[0])      ArrayStoreException: java.lang.Integer
//! LinkedList.toArray(new String[0])   ArrayStoreException: java.lang.Integer
//! ```
//!
//! because `ArrayList.toArray(T[])` copies through `Arrays.copyOf` /
//! `System.arraycopy` while `AbstractCollection.toArray(T[])` stores through
//! `aastore` in its own loop. The caller states which shape it is; this module
//! does not guess, because the receiver alone cannot tell it.
//!
//! # The predicate is the VM's, not a new one
//!
//! [`NativeContext::aastore_element_assignable`] is the same check the
//! `aastore` opcode, the JIT's `jit_aastore` and `java.lang.reflect.Array.set`
//! use. It is deliberately ADDITIVE — it fails open for interface components,
//! `$Proxy` values, synthetic class ids and cross-loader same-named components
//! — so routing through it cannot manufacture a false `ArrayStoreException`.
//! `None` means a context with no class hierarchy (the mocks), where the store
//! stays unchecked.

use crate::registry::NativeContext;
use cratonvm_types::error::{arraycopy_message, MethodCallFailed, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

/// Which JDK code path the native is standing in for, and therefore which
/// `ArrayStoreException` text it owes.
#[derive(Clone, Copy, Debug)]
pub enum StoreRoute<'a> {
    /// `ArrayList.toArray(T[])` — copies via `Arrays.copyOf` /
    /// `System.arraycopy`, so arraycopy's sentence. The source component is
    /// the copied array's, which for `ArrayList.elementData` is
    /// `java/lang/Object` whatever the list's element type.
    Arraycopy { source_component: &'a str },
    /// `AbstractCollection.toArray(T[])` — stores through `aastore` in its own
    /// loop, so the bare class name of the offending VALUE.
    Aastore,
}

/// `Some(err)` when `value` may not be stored into `array`; `None` when the
/// store is legal, when `value` is null, or when the context cannot decide.
///
/// Callers hold pins and locks the helper knows nothing about, so it RETURNS
/// the error rather than throwing: the call site stays responsible for
/// unwinding its own state before propagating.
#[must_use]
pub fn reject_unstorable(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
    value: Value,
    route: StoreRoute<'_>,
) -> Option<MethodCallFailed> {
    let Value::Object(Some(elem)) = value else {
        return None;
    };
    if ctx.aastore_element_assignable(array, elem) != Some(false) {
        return None;
    }
    let message = match route {
        StoreRoute::Arraycopy { source_component } => {
            // On a reference array the heap header's class id IS the component
            // class, so this is the destination COMPONENT and not the array.
            let dst = ctx
                .class_name_of_id(ctx.class_id_of_object(array))
                .unwrap_or_default();
            arraycopy_message::element_type_mismatch(source_component, &dst)
        }
        StoreRoute::Aastore => aastore_value_external_name(&*ctx, elem),
    };
    Some(RuntimeError::ArrayStoreException { message }.into())
}

/// HotSpot's `Klass::external_name()` of the VALUE a refused `aastore` tried to
/// store — the whole text of the `ArrayStoreException` that opcode throws.
///
/// # Why this is not `class_name_of_id(class_id_of_object(elem))`
///
/// On a reference array the heap header's class id holds the COMPONENT class,
/// not the array's own type (stated on
/// `vm::runtime::interpreter::typecheck::array_descriptor_of`, and again on
/// `cce_display_class_name`, where the same trap once produced
/// `java.lang.String cannot be cast to java.lang.String` and cost a session as
/// a supposed class-identity split). So the raw lookup is off by exactly one
/// array dimension for an array-valued element and right for everything else —
/// which is why only the array shapes ever diverged.
///
/// This was the module's one recorded KNOWN GAP ("`NativeContext` exposes no
/// element-type accessor to rebuild the descriptor from"). It does:
/// [`NativeContext::object_is_array`] and
/// [`NativeContext::heap_element_type_of`] are heap object-kind reads that do
/// not go through the class table at all, which is exactly what the component
/// -vs- array ambiguity needs. Same rebuild rule as `array_descriptor_of`.
///
/// MEASURED on HotSpot 25.0.3+9-LTS (`scratchpad` probe `AseDoors`, one
/// execution per shape so nothing is in the fast-throw regime):
///
/// ```text
/// Arrays.fill((Object[]) new String[3],   Integer.valueOf(1))  ArrayStoreException: java.lang.Integer
/// Arrays.fill((Object[]) new String[3][], new Integer[0])      ArrayStoreException: [Ljava.lang.Integer;
/// Arrays.fill((Object[]) new I[3],        new Object())        ArrayStoreException: java.lang.Object
/// ```
///
/// The middle row is the one a component-name answer gets wrong: it would say
/// `java.lang.Integer`, naming a type the store never mentioned.
///
/// `Klass::external_name()` on an `ObjArrayKlass` is the dotted DESCRIPTOR
/// (`[Ljava.lang.Integer;`), not the source form `java.lang.Integer[]` — so
/// this deliberately does NOT route through
/// [`arraycopy_message::external_class_name`], which renders the source form
/// for the arraycopy sentence's benefit. The two wordings are different on
/// purpose; `System.arraycopy`'s message really does say
/// `java.lang.String[]` where `aastore`'s says `[Ljava.lang.String;`.
#[must_use]
pub fn aastore_value_external_name(ctx: &dyn NativeContext, elem: ObjectRef) -> String {
    aastore_value_internal_name(ctx, elem).replace('/', ".")
}

/// [`aastore_value_external_name`]'s answer before the `/` -> `.` pass, in JVMS
/// internal form (`[Ljava/lang/Integer;`, `[I`, `java/lang/String`).
fn aastore_value_internal_name(ctx: &dyn NativeContext, elem: ObjectRef) -> String {
    let class_name = || {
        ctx.class_name_of_id(ctx.class_id_of_object(elem))
            .unwrap_or_default()
    };
    // A context with no heap (the mocks) answers `false` here, which lands on
    // the plain-class arm — the same answer this function gave before the array
    // arm existed, so nothing that used to work changes shape.
    if !ctx.object_is_array(elem) {
        return class_name();
    }
    match ctx.heap_element_type_of(elem) {
        ArrayElementType::Boolean => "[Z".to_string(),
        ArrayElementType::Char => "[C".to_string(),
        ArrayElementType::Float => "[F".to_string(),
        ArrayElementType::Double => "[D".to_string(),
        ArrayElementType::Byte => "[B".to_string(),
        ArrayElementType::Short => "[S".to_string(),
        ArrayElementType::Int => "[I".to_string(),
        ArrayElementType::Long => "[J".to_string(),
        ArrayElementType::Reference => {
            let comp = class_name();
            if comp.is_empty() {
                // No component entry: `array_descriptor_of` substitutes
                // `Object[]` for exactly this case, so say the same thing.
                "[Ljava/lang/Object;".to_string()
            } else if comp.starts_with('[') {
                // Already an array class name — one more dimension.
                format!("[{comp}")
            } else {
                format!("[L{comp};")
            }
        }
    }
}
