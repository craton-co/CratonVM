// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z11/nd: the natives the VM's native funnel may RE-RUN after a
//! collection when one of their allocations refused.
//!
//! # Why
//!
//! A native's allocation doors (`NativeContext::new_array`, `new_ref_array`,
//! `alloc_object`, the string factories, mirror minting) make ONE attempt: the
//! native holds raw `ObjectRef`s in Rust locals that no root set names, so the
//! door cannot collect. On a refusal it unwinds with the `NativeAllocOom`
//! payload, and the funnel (`vm_exec::safe_native_call_impl`) turned that into
//! `OutOfMemoryError` -- with the heap full of garbage a collection would have
//! freed. HotSpot never raises a heap `OutOfMemoryError` from a VM allocation
//! without a collection that clears soft references first
//! (`docs/internal/gc/zgc-zr8nr-one-attempt-native-doors-remaining-20261013.md`).
//!
//! At the funnel the native is GONE: its locals unwound, and every argument
//! is still pinned (`native_pin_roots[pin_base..]`). So the funnel can collect
//! there safely and then call the native AGAIN with the re-read arguments --
//! provided that running the native from the start a second time is the same
//! as running it once. That is a property of the native's BODY, so the list
//! below names bodies (callback addresses), never `(class, method,
//! descriptor)` triples: a triple re-registered with another body is simply
//! not in the list (safe: it keeps the one-attempt answer).
//!
//! # The admission rule (audited per body; keep it when adding one)
//!
//! A body is restartable when everything it does before its LAST allocation
//! is one of
//!
//! * reading VM state, the arguments or the heap;
//! * an idempotent VM action (linking or loading a class, `<clinit>`, a cache
//!   or census update, interning);
//! * allocating objects that are not yet reachable from anything but the
//!   native's own locals (they become garbage on the unwind);
//!
//! and it never mutates an argument, a Java-visible field of an existing
//! object, or runs arbitrary Java with user-visible side effects except class
//! loading (whose contract, JVMS 5.3.5, is idempotent per name and loader).
//! Paired state a body opens (a thread-local depth, a thread-dump marker)
//! must be closed on the unwind too, or the second run starts from the first
//! run's leftovers -- see `GET_DECLARED_METHODS_DEPTH` and `dumpThreads`.
//!
//! The VM consults [`native_is_oom_restartable`] only AFTER a refusal unwound,
//! so the list costs nothing on the dispatch path. Kill switch:
//! `CRATONVM_ZGC_NATIVE_OOM_REPLAY=0` on ZGC; `CRATONVM_GC_NATIVE_OOM_REPLAY=0` on
//! Generational and G1, which replay since gci i31/om (before, they never did).

use cratonvm_native_api::NativeCallback;

/// The restartable bodies (see the module doc for the admission rule).
///
/// * `Class.getDeclaredFields0` / `getDeclaredMethods0` /
///   `getDeclaredConstructors0` (and the `--compatible` no-argument rows that
///   share the body): link + optional signature loads, then fresh `Field` /
///   `Method` / `Constructor` objects into a fresh array. Reached again on a
///   near-full heap whenever the JDK's soft `ReflectionData` was cleared.
/// * `Class.getMethods` (`--compatible`): the same, over the hierarchy.
/// * `Class.getInterfaces0`, `getDeclaredClasses0`: fresh `Class[]` of
///   (possibly freshly loaded) mirrors.
/// * `Class.getName` / `initClassName`: an interned name String; and the
///   `Class.getName` LEAF body (`CRATONVM_CLASS_GET_NAME_MEMO`, default ON:
///   the funnel re-runs THAT callback when the leaf path declines), whose
///   only unwinding allocation is the same intern, made before the memo row
///   (`try_new_ref_array`, which refuses without unwinding) and its global
///   root; the argument pin it takes dies with the refused run. gci i31/om:
///   the Batch 3 G1 boot rows died of this body's one-attempt intern.
/// * `Thread.getStackTrace0` (another thread's `getStackTrace()`) and
///   `Thread.dumpThreads` (`Thread.getAllStackTraces()`): fresh
///   `StackTraceElement` arrays from a published frame snapshot.
/// * `Thread.getThreads`: a fresh `Thread[]` of the live platform threads.
/// * `--compatible` primitive formatters `Integer.toString(I)`,
///   `Long.toString(J)`, `Double.toString(D)`, `Integer.toHexString(I)`:
///   pure functions of a primitive argument.
/// * `Object.getClass`: reads the receiver, then mints (or finds) its class
///   mirror; the array arms used to take the one-attempt mirror door
///   (`docs/internal/gc/zgc-zr4ab-infallible-residuals-FIXED-20261017.md` item 7).
/// * `AbstractStringBuilder.getValue()[B` (`--compatible`, zs s1/nd): for a
///   synthetic (`char[]`) receiver it snapshots the units into Rust memory,
///   allocates ONE fresh `byte[]` through the one-attempt `new_array` door and
///   only then writes into that fresh array (`sb_put_units_at`); a compact
///   receiver is answered with its own payload and allocates nothing. Nothing
///   Java-visible changes before the allocation.
/// * `Integer.valueOf(I)` / `Long.valueOf(J)` (gci i47w/oe47): the registry
///   bodies (`lang_math`, `NativeKind::Intrinsic`) AND the interpreter
///   intrinsic table's trampolines for the same two kinds
///   (`intrinsics::callback_for`, the callback a cached interpreted
///   `invokestatic` site hands the funnel). Before the one allocation: the
///   primitive argument, the cache bound (an idempotent property read), the
///   cache table lookup and the wrapper class's (idempotent) initialization;
///   after it, only stores into the fresh wrapper and the cache CAS, which
///   cannot unwind. `GciGo11G1NamesProbe oome` on G1: the catch block's
///   `Integer.valueOf(n)`, right after the probe dropped its full list, met
///   the pre-drop heap through this one-attempt door and the
///   `OutOfMemoryError` escaped `main` with nothing collected (Generational
///   pays its OOME debt before the call instead, `pay_native_oome_debt`).
///
/// `native-builtins/tests/oom_restartable_ratchet.rs` freezes this list and a
/// census of each body's own field / array writes and Java calls: adding a
/// body, or a new effect in a listed body, fails CI until its audit row is
/// updated there.
pub fn restartable_bodies() -> [NativeCallback; 21] {
    [
        crate::lang_class::native_class_get_declared_fields,
        crate::lang_class::native_class_get_declared_methods,
        crate::lang_class::native_class_get_declared_constructors,
        crate::lang_class::native_class_get_methods,
        crate::lang_class::native_class_get_interfaces,
        crate::lang_class::native_class_get_declared_classes,
        crate::lang_class::native_class_get_name,
        crate::lang_class::native_class_get_name_leaf,
        crate::lang_system::native_thread_get_stack_trace,
        crate::native_thread_dump_threads,
        crate::native_thread_get_threads,
        crate::native_integer_to_string_compat,
        crate::native_long_to_string_compat,
        crate::native_double_to_string_compat,
        crate::native_integer_to_hex_string_compat,
        crate::native_object_get_class,
        crate::lang_string::native_sb_get_value,
        crate::lang_math::native_integer_value_of,
        crate::lang_math::native_long_value_of,
        crate::intrinsics::callback_for(cratonvm_native_api::InterpIntrinsic::IntegerValueOf),
        crate::intrinsics::callback_for(cratonvm_native_api::InterpIntrinsic::LongValueOf),
    ]
}

/// May the VM's native funnel collect and re-run `callback` after one of its
/// allocations refused? Asked on the refusal path only.
pub fn native_is_oom_restartable(callback: NativeCallback) -> bool {
    let addr = callback as usize;
    restartable_bodies().iter().any(|&f| f as usize == addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn not_listed(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[cratonvm_types::Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }

    /// Every listed body answers yes; an unlisted one (including a body that
    /// MUTATES its argument, `StringBuilder`-style) answers no.
    #[test]
    fn the_list_admits_exactly_its_bodies() {
        for f in restartable_bodies() {
            assert!(native_is_oom_restartable(f));
        }
        assert!(!native_is_oom_restartable(not_listed));
        // gci i31/om: the `Class.getName` LEAF body is the funnel's callback
        // under `CRATONVM_CLASS_GET_NAME_MEMO` (default ON).
        assert!(native_is_oom_restartable(
            crate::lang_class::native_class_get_name_leaf
        ));
        assert!(!native_is_oom_restartable(
            crate::lang_string::native_string_value_of_object
        ));
    }

    /// No body is listed twice (a duplicate would hide a deletion in review).
    #[test]
    fn no_body_is_listed_twice() {
        let mut addrs: Vec<usize> = restartable_bodies().iter().map(|&f| f as usize).collect();
        addrs.sort_unstable();
        let n = addrs.len();
        addrs.dedup();
        assert_eq!(addrs.len(), n);
    }

    /// gci i47w/oe47: `Integer.valueOf(I)` / `Long.valueOf(J)` are restartable
    /// through BOTH doors that reach the funnel -- the registry body and the
    /// interpreter intrinsic table's trampoline (the callback a cached
    /// interpreted `invokestatic` site passes) -- while the table's
    /// argument-mutating `StringBuilder` trampolines stay out.
    #[test]
    fn gci_i47w_oe47_boxing_value_of_is_restartable_through_both_doors() {
        use crate::intrinsics::callback_for;
        use cratonvm_native_api::InterpIntrinsic;
        assert!(native_is_oom_restartable(
            crate::lang_math::native_integer_value_of
        ));
        assert!(native_is_oom_restartable(
            crate::lang_math::native_long_value_of
        ));
        assert!(native_is_oom_restartable(callback_for(
            InterpIntrinsic::IntegerValueOf
        )));
        assert!(native_is_oom_restartable(callback_for(
            InterpIntrinsic::LongValueOf
        )));
        // The trampoline is one function per kind: asking twice answers the
        // same address the funnel compares against.
        assert_eq!(
            callback_for(InterpIntrinsic::IntegerValueOf) as usize,
            callback_for(InterpIntrinsic::IntegerValueOf) as usize
        );
        assert!(!native_is_oom_restartable(callback_for(
            InterpIntrinsic::StringBuilderAppendString
        )));
        assert!(!native_is_oom_restartable(callback_for(
            InterpIntrinsic::IntegerIntValue
        )));
    }
}
