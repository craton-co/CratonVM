// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.6 — synthetic `LambdaMetafactory` proxy metadata.
//!
//! The bulk of lambda proxy creation (bootstrap + allocation) lives in
//! `runtime/invokedynamic.rs` and the dispatch loop in
//! `runtime/interpreter.rs`. This module hosts the small pieces that are
//! shared between those two: descriptor parsing of SAM method types,
//! runtime diagnostics counters, and a few helpers for classifying the
//! `MethodHandleKind` of the implementation target.
//!
//! Keeping the helpers in a dedicated module keeps the invokedynamic file
//! focused on the bsm dispatch state machine and leaves a clean seam
//! where future WPs (e.g. dedicated inline-caches for lambda call sites)
//! can hook in.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

/// Category of the implementation target captured by `LambdaCallSite`.
///
/// This is what the SAM call-site ends up delegating to. Consumers use
/// this to decide how many pre-arguments the proxy will need and how to
/// route the `invokeSpecial`/`invokeVirtual`/etc. dispatch.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ImplKind {
    /// `REF_invokeStatic` — a static method reference. Captures are passed
    /// in as leading parameters.
    Static,
    /// `REF_invokeVirtual` / `REF_invokeInterface` — instance bind via
    /// `this` (captured as the first parameter of the indy factory).
    Virtual,
    /// `REF_invokeSpecial` — non-virtual instance dispatch (e.g. private
    /// helpers, `super.foo()` references).
    Special,
    /// `REF_newInvokeSpecial` — constructor reference (`Foo::new`).
    Constructor,
    /// `REF_getField` / `REF_getStatic` — field-getter reference.
    Getter,
    /// `REF_putField` / `REF_putStatic` — field-setter reference.
    Setter,
}

impl ImplKind {
    /// Classify the bsm's implementation MethodHandle.
    pub fn from_method_handle(mh: &MethodHandle) -> Self {
        match mh.kind {
            MethodHandleKind::InvokeStatic => ImplKind::Static,
            MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                ImplKind::Virtual
            }
            MethodHandleKind::InvokeSpecial => ImplKind::Special,
            MethodHandleKind::NewInvokeSpecial => ImplKind::Constructor,
            MethodHandleKind::GetField | MethodHandleKind::GetStatic => ImplKind::Getter,
            MethodHandleKind::PutField | MethodHandleKind::PutStatic => ImplKind::Setter,
        }
    }

    /// `true` iff this impl kind requires a non-static dispatch — i.e. the
    /// first argument to the impl will be the `this` of the captured
    /// instance.
    pub fn is_instance(self) -> bool {
        matches!(
            self,
            ImplKind::Virtual | ImplKind::Special | ImplKind::Getter | ImplKind::Setter
        )
    }
}

/// Diagnostic counters for lambda-proxy lifecycle.
///
/// These are purely advisory — the tests use them to assert that
/// bootstrap and dispatch paths are both exercised during a probe run.
static BOOTSTRAPPED: AtomicUsize = AtomicUsize::new(0);
static DISPATCHED: AtomicUsize = AtomicUsize::new(0);

/// Record that a lambda call-site was freshly bootstrapped. Called once
/// per distinct (class, cp_index) pair by `invokedynamic.rs`.
pub fn note_bootstrap() {
    BOOTSTRAPPED.fetch_add(1, Ordering::Relaxed);
}

/// Record that a lambda proxy was dispatched (SAM method invoked).
pub fn note_dispatch() {
    DISPATCHED.fetch_add(1, Ordering::Relaxed);
}

/// Cumulative bootstrap count since VM start.
pub fn bootstrap_count() -> usize {
    BOOTSTRAPPED.load(Ordering::Relaxed)
}

/// Cumulative dispatch count since VM start.
pub fn dispatch_count() -> usize {
    DISPATCHED.load(Ordering::Relaxed)
}

/// Describe a `LambdaCallSite` in a short human-readable form for log
/// output and test assertions. Format:
/// `<functional_interface>.<sam_method><sam_descriptor>` –> `<impl>`.
pub fn describe_call_site(lcs: &LambdaCallSite) -> String {
    format!(
        "{}.{}{} -> {}.{}{}",
        lcs.functional_interface,
        lcs.sam_method_name,
        lcs.sam_descriptor,
        lcs.impl_handle.class_name,
        lcs.impl_handle.member_name,
        lcs.impl_handle.descriptor,
    )
}

/// Return the number of capture arguments the lambda factory takes. This
/// is the length of the indy factory's parameter list.
pub fn capture_count(lcs: &LambdaCallSite) -> usize {
    lcs.capture_types.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::resolution::{MethodHandle, MethodHandleKind};

    fn mk_handle(kind: MethodHandleKind) -> MethodHandle {
        MethodHandle {
            kind,
            class_name: std::sync::Arc::from("com/example/Foo"),
            member_name: std::sync::Arc::from("bar"),
            descriptor: std::sync::Arc::from("()V"),
        }
    }

    #[test]
    fn impl_kind_classification_covers_every_reference_kind() {
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeStatic)),
            ImplKind::Static
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeVirtual)),
            ImplKind::Virtual
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeInterface)),
            ImplKind::Virtual
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeSpecial)),
            ImplKind::Special
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::NewInvokeSpecial)),
            ImplKind::Constructor
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::GetField)),
            ImplKind::Getter
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::PutStatic)),
            ImplKind::Setter
        );
    }

    #[test]
    fn is_instance_predicate_matches_spec() {
        assert!(ImplKind::Virtual.is_instance());
        assert!(ImplKind::Special.is_instance());
        assert!(ImplKind::Getter.is_instance());
        assert!(ImplKind::Setter.is_instance());
        assert!(!ImplKind::Static.is_instance());
        assert!(!ImplKind::Constructor.is_instance());
    }

    #[test]
    fn counters_are_monotonic() {
        let before_b = bootstrap_count();
        let before_d = dispatch_count();
        note_bootstrap();
        note_bootstrap();
        note_dispatch();
        assert!(bootstrap_count() >= before_b + 2);
        assert!(dispatch_count() >= before_d + 1);
    }
    /// Guards the JDK's serializability rule for lambda proxies
    /// (`SharedVm::lambda_proxy_serializability`).
    ///
    /// Before 2026-08-01 every lambda proxy was treated as `Serializable`:
    /// `getDeclaredMethods()` reported a synthetic `writeReplace()` on all of
    /// them and the `instanceof` fast path answered `true` for
    /// `java.io.Serializable` unconditionally, so a plain
    /// `Supplier<String> s = () -> "x"` looked serializable where real HotSpot
    /// throws ClassCastException on `(Serializable) s`.
    ///
    /// This asserts the two arms a bare fixture can decide without a loaded
    /// class hierarchy: the recorded `FLAG_SERIALIZABLE` alone must drive
    /// `ByFlag` vs `NotSerializable`, and a `ClassId` that is not a registered
    /// proxy at all must come back `NotSerializable` rather than defaulting to
    /// serializable. (`ByInheritance` needs a real `Serializable`-extending
    /// interface loaded, so the `LambdaSerProbe` differential probe against
    /// jdk-25 covers that arm.)
    ///
    /// NB: the sibling lambda-proxy tests in `vm/src/vm.rs` live behind
    /// `#[cfg(all(test, feature = "synthetic-jdk"))]` and do NOT run by
    /// default -- this one is deliberately here so it actually executes.
    #[test]
    fn lambda_proxy_serializability_follows_the_recorded_flag() {
        use crate::classloading::resolution::LambdaCallSite;
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use cratonvm_native_api::LambdaSerializability;
        use cratonvm_types::ClassId;
        use std::sync::Arc;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        let register = |flag: bool| {
            let id = shared.alloc_lambda_proxy_id();
            shared.classes.lambda_proxies.write().insert(
                id,
                std::sync::Arc::new(LambdaCallSite {
                    functional_interface_id: None,
                    // `Runnable` does not extend `Serializable`, so the
                    // inheritance half of the rule is false either way and the
                    // recorded flag is the only thing under test.
                    functional_interface: std::sync::Arc::from("java/lang/Runnable"),
                    sam_method_name: std::sync::Arc::from("run"),
                    sam_descriptor: std::sync::Arc::from("()V"),
                    impl_handle: mk_handle(MethodHandleKind::InvokeStatic),
                    instantiated_descriptor: std::sync::Arc::from("()V"),
                    capture_types: vec![],
                    proxy_class_id: id,
                    serializable_flag: flag,
                }),
            );
            id
        };

        let plain = register(false);
        let ser = register(true);

        assert_eq!(
            shared.lambda_proxy_serializability(plain),
            LambdaSerializability::NotSerializable,
            "a plain metafactory lambda over a non-Serializable interface must not \
             be serializable"
        );
        assert_eq!(
            shared.lambda_proxy_serializability(ser),
            LambdaSerializability::ByFlag,
            "FLAG_SERIALIZABLE must make the lambda serializable AND earn it the \
             added java.io.Serializable marker interface"
        );
        assert_eq!(
            shared.lambda_proxy_serializability(ClassId::new(1)),
            LambdaSerializability::NotSerializable,
            "a ClassId that is not a registered lambda proxy must not be reported \
             serializable"
        );
    }
}
