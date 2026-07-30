// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Precise JVMTI-redefinition state for interpreter caches and JIT gates.
//!
//! The process-wide flag is only a cheap negative fast path. Positive
//! decisions are scoped to an exact class or receiver hierarchy, so redefining
//! one class cannot permanently disable unrelated caches or compilation.

use crate::classloading::ClassManager;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

#[inline]
pub(crate) fn class_was_redefined(shared: &SharedVm, class_id: ClassId) -> bool {
    crate::classloading::any_class_redefined()
        && shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation(class_id)
            > 0
}

#[inline]
pub(crate) fn named_class_was_redefined(shared: &SharedVm, class_name: &str) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    cm.get_loaded_class_id(class_name)
        .is_some_and(|class_id| cm.class_redefine_generation(class_id) > 0)
}

/// Prefer a loader-qualified id when it names the expected class, falling back
/// to a unique name lookup for legacy/static invoke descriptors whose id field
/// is not populated.
#[inline]
pub(crate) fn class_id_or_name_was_redefined(
    shared: &SharedVm,
    class_id_raw: u32,
    class_name: &str,
) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    let candidate = ClassId::new(class_id_raw);
    if cm
        .get_class(candidate)
        .is_some_and(|class| class.name.as_ref() == class_name)
    {
        return cm.class_redefine_generation(candidate) > 0;
    }
    cm.get_loaded_class_id(class_name)
        .is_some_and(|class_id| cm.class_redefine_generation(class_id) > 0)
}

/// Exact-class check for call sites that already hold the class-manager read
/// guard. This avoids a non-reentrant nested read while a writer is queued.
#[inline]
pub(crate) fn native_shadow_suppressed_in(cm: &ClassManager, class_name: &str) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    cm.get_loaded_class_id(class_name)
        .is_some_and(|class_id| cm.class_redefine_generation(class_id) > 0)
}

#[inline]
pub(crate) fn hierarchy_fingerprint_in(cm: &ClassManager, receiver_class_id: ClassId) -> u64 {
    if !crate::classloading::any_class_redefined() {
        return 0;
    }
    let mut fingerprint = 0u64;
    let mut current = Some(receiver_class_id);
    for _ in 0..32 {
        let Some(class_id) = current else {
            break;
        };
        let generation = cm.class_redefine_generation(class_id);
        if generation != 0 {
            fingerprint = fingerprint
                .rotate_left(11)
                .wrapping_add(((class_id.as_u32() as u64) << 32) | generation as u64);
        }
        current = cm.get_class(class_id).and_then(|class| class.superclass);
    }
    fingerprint
}

#[inline]
pub(crate) fn hierarchy_was_redefined(shared: &SharedVm, receiver_class_id: ClassId) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    hierarchy_fingerprint_in(&shared.classes.class_manager.read(), receiver_class_id) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_fingerprint_is_zero_before_any_redefinition() {
        let cm = ClassManager::new(&[], &[], &[]);
        assert_eq!(hierarchy_fingerprint_in(&cm, ClassId::new(1)), 0);
    }
}
