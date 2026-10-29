// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The one builder of natively thrown exceptions (JIT round 21 wave 19, lane
//! jnilazy; proposal JO4-1 of
//! `docs/internal/jit-proposals/jit-r15w4-jdkonly4-proposals-RETIRED-20261003.md`,
//! page `docs/internal/fixed-bugs/jr21-jnilazy-throwable-builder-and-ratchet-FIXED-20261027.md`).
//!
//! The rule it holds, learned in rounds 15 waves 3-4 (`JO-2`, `JO3-2`):
//!
//! * an exception is built through its constructor with the arguments rooted
//!   across `<init>` and the FORWARDED reference answered
//!   ([`NativeHeapAccess::construct_throwable`]) -- never `new_object` followed
//!   by `invoke("<init>")`, which answers the pre-constructor address (stale
//!   after a moving collection inside the constructor, which allocates at
//!   least the stack trace) and throws a half-built object when the
//!   constructor fails;
//! * a constructor that THREW is the answer (HotSpot's `new X(..)` in the
//!   JDK's throw path propagates an `OutOfMemoryError` from
//!   `fillInStackTrace`, or an application override's throwable); only "the
//!   class does not resolve / has no such constructor" falls back. Plain
//!   [`NativeHeapAccess::new_object_initialized`] answers `Err` for both, so a
//!   site that builds a throwable with it raises its fallback where HotSpot
//!   propagates.
//!
//! Before this module the rule lived twice -- `native-builtins`'
//! `throwable_mint::construct_thrown` and `native-io`'s
//! `nio_native::construct_thrown` -- because neither crate depends on the
//! other; both are now one-line forwards to [`throw_new`]. The gate that
//! keeps new sites on it is `native-builtins/tests/throwable_mint_ratchet.rs`:
//! it counts the raw shapes (a literal throwable class handed to
//! `new_object` / `new_object_initialized`, a direct `construct_throwable`
//! call) in every native crate except this file, frozen exactly.

use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::{ObjectRef, Value};

use crate::NativeContext;

/// `new class(init_args..)` for a natively thrown exception, with the
/// three-way answer of [`crate::NativeHeapAccess::construct_throwable`]:
///
/// * `Ok(Some(exc))`: constructed; `exc` is current (forwarded);
/// * `Ok(None)`: it cannot be built at all (no such class or constructor, e.g.
///   a synthetic image) -- the caller decides its fallback (a mint, a flat
///   error);
/// * `Err(thrown)`: the class resolved and its constructor threw; propagate
///   `thrown`, as HotSpot does.
///
/// `init_args` are rooted across `<init>` by the VM. `class` is an internal
/// name (`java/lang/IllegalStateException`), `init_desc` the constructor's
/// descriptor (`(Ljava/lang/String;)V`).
pub fn construct_new(
    ctx: &mut dyn NativeContext,
    class: &str,
    init_desc: &str,
    init_args: &[Value],
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    ctx.construct_throwable(class, init_desc, init_args)
}

/// `throw new class(init_args..)` from native code: the failure to return.
/// The constructed exception as `ExceptionThrown`, or the constructor's own
/// throwable when it threw, or `fallback()` when the object cannot be built at
/// all ([`construct_new`]'s `Ok(None)`). The fallback is a closure, so no
/// caller can drop the third case by matching on two.
pub fn throw_new(
    ctx: &mut dyn NativeContext,
    class: &str,
    init_desc: &str,
    init_args: &[Value],
    fallback: impl FnOnce() -> MethodCallFailed,
) -> MethodCallFailed {
    match construct_new(ctx, class, init_desc, init_args) {
        Ok(Some(exc)) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
        Ok(None) => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_mock::MockNativeContext;

    /// The mock's constructor path succeeds: the constructed object is the
    /// answer and the fallback never runs.
    #[test]
    fn a_constructed_throwable_is_thrown_and_the_fallback_is_not_asked() {
        let mut ctx = MockNativeContext::new();
        let failed = throw_new(
            &mut ctx,
            "java/lang/IllegalStateException",
            "()V",
            &[],
            || panic!("the constructor built the object; the fallback must not run"),
        );
        assert!(matches!(failed, MethodCallFailed::ExceptionThrown(_)));
        let built = construct_new(&mut ctx, "java/lang/IllegalStateException", "()V", &[]);
        assert!(matches!(built, Ok(Some(_))));
    }
}
