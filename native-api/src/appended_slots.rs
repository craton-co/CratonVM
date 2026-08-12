// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Where a native's **private** slot map is allowed to start on an instance of
//! a real JDK class.
//!
//! # The species this closes
//!
//! `docs/architecture/natives-over-real-jdk-classes.md` §5: a native that keeps
//! its own state in an object's fields, indexed from 0, is stating a layout. On
//! a fabricated stub that statement is true — the stub's fields *are* `_f0.._fN`
//! and the native's map is the layout. On the real class it is false, and slot
//! `i` is whatever the real class declares there.
//!
//! The remedy is not to renumber by hand. It is to start the private map ABOVE
//! every field the real class declares, so no private slot can ever collide
//! with a real one, and to collapse the base to 0 exactly when the class is a
//! stub.
//!
//! # Why this lives in `native-api` rather than in one native crate
//!
//! It was written in `native-builtins/src/util_concurrent_ext.rs` by
//! W7-49-slot-index-recensus.md, next to that crate's allocation funnel. But
//! the sites that need it are not confined to that crate:
//! W7-68-live-under-allocations.md found the `under` direction's live cases in
//! `native-io` (`java/nio/channels/FileChannel`, `java/nio/MappedByteBuffer`),
//! which cannot reach a `pub(crate)` helper in `native-builtins`.
//!
//! Copying it would have made it the sixteenth private re-implementation of a
//! primitive W7-59-layout-detector-coverage.md §2.1 counted fifteen copies of
//! — *"that is the same primitive re-implemented fifteen times, which is
//! exactly why instrumenting call sites was never going to work"*. So the
//! implementation moved here, once, and `native-builtins` forwards to it. Every
//! method it needs (`is_class_synthetic_stub`, `class_id_by_name`,
//! `class_num_total_fields`, `ensure_class_initialized`) is already on
//! `NativeContext`; nothing new is required of any implementor.
//!
//! # The stub arm is load-bearing
//!
//! Asking `class_num_total_fields` unconditionally — which the four private
//! `synthetic_base_offset` copies in `native-builtins/src/jca/` do — **ratchets**
//! in synthetic-JDK mode: the first allocation fabricates a class declaring
//! `base + width` fields, and the next call reads that number back as the new
//! base. Two objects of one class then carry two different slot maps in one
//! run, which is the very condition this module exists to prevent.
//! `is_class_synthetic_stub` is stable under that: a stub stays a stub.
//!
//! # What it cannot do
//!
//! It cannot make a native safe against a receiver it did **not** allocate.
//! W7-49 §8 records why, and the reasoning is not repeated here: given a
//! foreign object, "how many private slots does it carry" is unanswerable from
//! its width alone. A per-class base is only sound where every receiver the
//! native sees is an instance of that exact class — which is a claim about
//! registration and about which subclasses declare the method with `Code`, and
//! has to be made per call site, in place.

use crate::registry::NativeContext;

/// The private-slot base for `class_name`, for a caller that only holds `&dyn`.
///
/// Answers from the class as currently loaded and does **not** load it. Use
/// this on the accessor paths, where the object in hand was allocated by a
/// sibling native that already loaded the class; use [`base_for_class`] on the
/// allocation path, where it may not be loaded yet.
///
/// Returns 0 for a fabricated stub (the private map IS the layout) and 0 for a
/// class that is not loaded — the same answer the allocation path would have
/// reached, so an accessor and its allocator cannot disagree.
#[must_use]
pub fn base_for_loaded_class(ctx: &dyn NativeContext, class_name: &str) -> usize {
    if ctx.is_class_synthetic_stub(class_name) {
        return 0;
    }
    match ctx.class_id_by_name(class_name) {
        Some(cid) => ctx.class_num_total_fields(cid),
        None => 0,
    }
}

/// The private-slot base for `class_name`, loading and initialising it first.
///
/// The allocation-path form: the width a caller then asks for is
/// `base + width`, which is what makes the private slots both in bounds and
/// non-aliasing.
#[must_use]
pub fn base_for_class(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    if ctx.is_class_synthetic_stub(class_name) {
        return 0;
    }
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.class_num_total_fields(cid),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two forms must agree, because an allocator and an accessor that
    /// disagree about the base is precisely the two-layouts-on-one-class
    /// condition this module exists to prevent. `MockNativeContext` reports no
    /// classes and no stubs, so both answer 0 — the assertion worth having here
    /// is that neither has an arm the other lacks.
    #[test]
    fn the_immutable_and_loading_forms_agree_on_an_unknown_class() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        let immutable = base_for_loaded_class(&ctx, "does/not/Exist");
        let loading = base_for_class(&mut ctx, "does/not/Exist");
        assert_eq!(immutable, loading);
        assert_eq!(immutable, 0, "an unloadable class carries no real fields");
    }
}
