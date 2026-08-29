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
use cratonvm_types::{ObjectRef, Value};

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
        StoreRoute::Aastore => {
            // KNOWN GAP, pinned by `probes/DodArrayStoreSweep` rather than
            // hidden: for a value that is ITSELF AN ARRAY this names the
            // COMPONENT, because `class_id_of_object` answers the component for
            // any array and `NativeContext` exposes no element-type accessor to
            // rebuild the descriptor from. Throwing with a slightly imprecise
            // name is strictly better than not throwing.
            let vn = ctx
                .class_name_of_id(ctx.class_id_of_object(elem))
                .unwrap_or_default();
            arraycopy_message::external_class_name(&vn)
        }
    };
    Some(RuntimeError::ArrayStoreException { message }.into())
}
