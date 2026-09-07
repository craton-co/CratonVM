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

// `NativeClassAccess` is the trait that actually declares
// `is_class_synthetic_stub` / `ensure_class_initialized` /
// `class_num_total_fields`; `NativeContext` is the empty aggregate over the
// access traits, so it alone does not bring those methods into scope.
use crate::registry::{NativeClassAccess, NativeContext, NativeHeapAccess};
use cratonvm_types::ClassId;

/// The private-slot base for `class_name`, loading it first if it is not
/// already loaded.
///
/// Zero when the class is a fabricated stub — its fields are `_f0.._fN` and the
/// private map IS the layout — and otherwise the real class's transitive
/// declared field count, so `base + i` for every private `i` lands above every
/// field the class declares. The width a caller then asks the allocator for is
/// `base + width`, which is what makes those slots both in bounds and
/// non-aliasing.
///
/// # The already-loaded arm does not run `<clinit>`, and cannot disagree
///
/// A `&dyn`-taking sibling that answered *only* from `class_id_by_name` was
/// written first and removed, for a reason that still stands: on a miss it
/// answered 0 where this function answers the real count, and an accessor and
/// its allocator that disagree about the base are *exactly* the
/// two-layouts-on-one-class condition this module exists to prevent, arrived at
/// from the other direction.
///
/// The `class_id_by_name` arm below is not that sibling. It **falls through**
/// to `ensure_class_initialized` on a miss, so the two can only ever differ
/// when the lookup answers `Some` — and `Some` is exactly the case where they
/// cannot. The VM implements it as `find_unique_class_by_name`, which fails
/// **closed** on a name several loaders define: `None`, never one of the
/// candidates. That is what
/// [`classify_class_name`](NativeClassAccess::classify_class_name) exists to
/// disambiguate, and its doc states the rule this arm relies on — a plain
/// `None` means *absent OR ambiguous*. So `Some(cid)` says the name resolves to
/// exactly one class, which is the one `ensure_class_initialized` would have
/// returned, and a genuinely ambiguous name takes the old path unchanged.
///
/// # What it buys
///
/// `ensure_class_initialized` runs `<clinit>` — arbitrary Java, a GC point, and
/// a re-entry that can move or (under the generational young sweep) zero every
/// unpinned `ObjectRef` the calling native is holding. Every private-slot
/// accessor in `native-io` reaches this function, so *a plain-looking private
/// field read was a collection point*: 45 rows of
/// `docs/internal/audits/wide-tranche-triage-20260907.md` are rooted here and
/// nowhere else.
///
/// The count never needed `<clinit>` to be correct. `num_total_fields` is
/// computed by `compute_field_layout` when the class is **defined**, and
/// `class_manager` asserts that even `redefine_class` leaves it unchanged, so
/// initialisation cannot move the number this function returns. Skipping it
/// changes when `<clinit>` runs, not what the base is.
///
/// This is not the whole remedy: a class that is genuinely not loaded yet still
/// falls through, and must — answering 0 for a real class would lay the private
/// map over its declared fields. [`base_for_class_id`] is the arm for callers
/// that already hold a resolved id and therefore need none of this.
#[must_use]
pub fn base_for_class(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    if ctx.is_class_synthetic_stub(class_name) {
        return 0;
    }
    if let Some(cid) = ctx.class_id_by_name(class_name) {
        return ctx.class_num_total_fields(cid);
    }
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.class_num_total_fields(cid),
        Err(_) => 0,
    }
}

/// [`base_for_class`] for a caller that already holds the resolved class id.
///
/// Takes `&dyn`, not `&mut dyn`, and that is the point rather than an
/// incidental tightening: every method it calls is `&self`, so the signature is
/// a compile-time statement that **no GC can run inside it**. An edit that
/// later reached for `ensure_class_initialized`, `alloc_object` or any Java
/// re-entry would fail to borrow — the only kind of guarantee that survives
/// this file being read by someone who has not read this comment.
///
/// # Why an id-taking form is safe where the removed name-taking one was not
///
/// The sibling [`base_for_class`] describes was unsound because it could answer
/// a *different number* than the allocator did for the *same name*. This one is
/// handed the identity, so there is nothing left to resolve and nothing to
/// disagree about: `class_num_total_fields` is a plain read of the class
/// record's define-time count. Where the caller took that id from an object
/// (`class_id_of_object`), it is strictly MORE faithful than the name
/// round-trip it replaces — on an ambiguous name that round-trip could return a
/// same-named sibling's field count and index the private map off a class the
/// receiver is not an instance of.
///
/// The stub arm is kept and is still load-bearing: on a fabricated stub the
/// fields ARE `_f0.._fN` and the private map IS the layout, so the base must
/// collapse to 0 or it ratchets. See this module's header.
#[must_use]
pub fn base_for_class_id(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    match ctx.class_name_of_id(class_id) {
        // An id naming no loaded class carries no known real fields, so the
        // base collapses to 0 — the same answer the stub arm gives, and the
        // same answer `base_for_class` gives a name that will not resolve.
        Some(name) if !ctx.is_class_synthetic_stub(&name) => ctx.class_num_total_fields(class_id),
        _ => 0,
    }
}

/// The private-slot base for the object `this`, with the width guard that says
/// "this receiver is not one I allocated".
///
/// The accessor-side twin of [`base_for_class`], and the guard is load-bearing
/// in BOTH directions:
///
///   * a receiver the native did NOT allocate — a real `sun.nio.ch.*Impl` built
///     by JDK bytecode, or a stub-mode object — is too narrow for
///     `base + width`, so the base collapses to 0 and the accessor reads
///     exactly the slots it read before the private map moved. Never a new
///     refusal, never an out-of-range access.
///   * an allocator that failed to resolve its class and fell back to the
///     untyped sentinel gets a `cratonvm/synthetic/AnonymousObject$N`
///     substitute declaring exactly `width` fields; a later `base_for_class` on
///     that receiver would answer `width` and disagree with the 0 the allocator
///     used. The width check sends it back to 0, which is the base that was
///     actually used.
///
/// Written out three times (`pipe.rs::channel_private_base`,
/// `native-io/src/concrete_receiver.rs`, and this) before it moved here; the
/// two callers now forward.
///
/// # It asks the receiver's OWN class, and takes `&dyn`
///
/// It used to read the class *name* back out of the id and hand that name to
/// [`base_for_class`], which resolved it a second time. That round-trip cost an
/// `ensure_class_initialized` — on the accessor side, so an ordinary private
/// field read became a `<clinit>` door — and on an ambiguous name it could land
/// on a different class than the receiver's. The id is already in hand and is
/// the receiver's own, so [`base_for_class_id`] removes the GC point and the
/// ambiguity in one step. The `&dyn` signature is what keeps it removed.
#[must_use]
pub fn base_for_object(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    width: usize,
) -> usize {
    let base = base_for_class_id(ctx, ctx.class_id_of_object(this));
    if ctx.object_num_fields(this) >= base + width {
        base
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A class that cannot be resolved carries no known real fields, so the
    /// base collapses to 0 and the private map is the layout — the same answer
    /// the stub arm gives. Both arms must reach it, because a base that
    /// silently became non-zero for an unresolvable class would push every
    /// private slot past the object.
    #[test]
    fn an_unresolvable_class_has_a_zero_base() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        assert_eq!(base_for_class(&mut ctx, "does/not/Exist"), 0);
    }

    /// A class that IS loaded is answered from the loaded class, without
    /// `ensure_class_initialized`.
    ///
    /// This is a positive control for the arm, not merely for the number. The
    /// mock's `ensure_class_initialized` answers `ClassId::new(0)` for every
    /// name — an id it never declares — so the fallback path can only ever
    /// return 0 here. Deleting the `class_id_by_name` arm turns this assertion
    /// from 3 into 0. That is the whole point: a fast path that is never taken
    /// looks exactly like a correct one when both arms agree, and this file's
    /// sibling `scripts/unpinned-native-local-audit.py` carries the same lesson
    /// written three times.
    #[test]
    fn a_loaded_class_is_answered_without_initialising_it() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        ctx.declare_class(
            "p/Loaded",
            &[("a", "I"), ("b", "J"), ("c", "Ljava/lang/Object;")],
        );
        assert_eq!(
            base_for_class(&mut ctx, "p/Loaded"),
            3,
            "the loaded arm must answer from the loaded class id"
        );
    }

    /// [`base_for_class_id`] gives the same base as [`base_for_class`] for the
    /// same class, and 0 for an id that names nothing.
    ///
    /// The two must not be allowed to drift: an accessor on the id form and an
    /// allocator on the name form disagreeing about the base is the
    /// two-layouts-on-one-class condition this module exists to prevent.
    #[test]
    fn the_id_form_and_the_name_form_agree() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        let cid = ctx.declare_class("p/Both", &[("x", "I"), ("y", "I")]);
        assert_eq!(base_for_class_id(&ctx, cid), 2);
        assert_eq!(base_for_class_id(&ctx, cid), base_for_class(&mut ctx, "p/Both"));
        assert_eq!(
            base_for_class_id(&ctx, cratonvm_types::ClassId::new(9999)),
            0,
            "an id naming no loaded class must collapse to 0, not index past the object"
        );
    }

    /// The width guard still collapses the base on a receiver too narrow to
    /// carry the private map — the direction that keeps a foreign receiver
    /// reading exactly the slots it read before this module existed.
    ///
    /// Asserted through [`base_for_object`] rather than by inspection because
    /// that function no longer round-trips the class NAME, and the guard is the
    /// only thing left standing between a wide base and an out-of-range read.
    #[test]
    fn a_receiver_too_narrow_for_the_private_map_collapses_to_zero() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        let cid = ctx.declare_class("p/Narrow", &[("a", "I"), ("b", "I")]);
        // Allocated at the bare declared width: no room for `base + width`.
        let narrow = ctx.alloc_object(cid, 2);
        assert_eq!(base_for_object(&ctx, narrow, 3), 0);
        // Allocated the way this module's allocators do — `base + width`.
        let wide = ctx.alloc_object(cid, 2 + 3);
        assert_eq!(base_for_object(&ctx, wide, 3), 2);
    }
}
