// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK-only mode — the native-vs-bytecode decision point
//! (`docs/feature-designs/jdk-only-mode.md` §7).
//!
//! `resolve_dispatch` is specified as **the** single place the VM decides
//! between running a method's own bytecode and calling a registered native.
//! Interpreter, JIT, reflection, JNI and method handles all route through it.
//! That centralisation is the point: this repository's history is full of bugs
//! caused by the decision being re-implemented per call path — a native that
//! shadows real bytecode at one dispatch site and not another produces
//! behaviour that changes depending on whether a method was JIT-compiled.
//!
//! The normative order (contract §7) is:
//!
//! 1. `ACC_NATIVE` → the registered bridge or intrinsic. Under `JdkOnly` a
//!    `SyntheticStub` is `Reject(SyntheticNativeInvocation)`; nothing
//!    registered at all is `Reject(MissingNative)` — never a stub.
//! 2. a registered `Intrinsic` wins next (it is a *reviewed*,
//!    semantics-preserving fast path, so it may shadow bytecode),
//! 3. concrete bytecode beats a registered `Bridge` or `SyntheticStub`,
//! 4. otherwise `Reject(MissingImplementation)`.
//!
//! **Scope note.** Steps 2–4 are asserted under the strict policy, where the
//! contract is unambiguous. Under `Compatible` this file asserts only the
//! property wave 1 actually promises — that *no policy refusal is produced* —
//! and deliberately does **not** pin which of bytecode/native wins, because
//! wave 1's job there is to preserve today's behaviour bit-for-bit, including
//! the hard-coded exception lists that are marked `// JDK-ONLY-WAVE2:` for
//! mechanical removal later.
//!
//! `resolve_dispatch` is a pure function of its four arguments, so the tests
//! build a `Class` through `ClassManager` and construct methods standalone;
//! no VM is booted, no JDK image is needed and no Cargo feature is involved.

use cratonvm_classloading::{Class, ClassManager};
use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind};
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::ByteView;
use cratonvm_types::compat::{CompatibilityMode, ExecutionPolicy};
use cratonvm_types::error::{JdkOnlyViolation, MethodCallResult};
use cratonvm_types::Value;
use cratonvm_vm::vm::{resolve_dispatch, DispatchDecision};

const OWNER: &str = "com/example/Dispatch";

fn native_cb(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(7)))
}

/// A `ClassManager` holding one class we can borrow as the `&Class` argument.
/// `resolve_dispatch` reads the class for provenance and naming only, so the
/// class need not literally declare the method under test.
fn owner_class(mgr: &mut ClassManager) -> &Class {
    let id = mgr
        .try_ensure_synthetic_class(OWNER, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
    mgr.get_class(id)
        .expect("just-created class is in the store")
}

/// An `ACC_NATIVE` method: no `Code` attribute, by definition.
fn native_method(name: &str, descriptor: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
        name: name.into(),
        descriptor: descriptor.into(),
        attributes: vec![],
    }
}

/// A concrete method with a decoded `Code` attribute (`iconst_1; ireturn`).
fn bytecode_method(name: &str, descriptor: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: name.into(),
        descriptor: descriptor.into(),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: ByteView::from_vec(vec![0x04, 0xAC]),
            exception_table: vec![],
            attributes: vec![],
        }))],
    }
}

/// An abstract method: neither `ACC_NATIVE` nor a `Code` attribute.
fn abstract_method(name: &str, descriptor: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
        name: name.into(),
        descriptor: descriptor.into(),
        attributes: vec![],
    }
}

fn registered(kind: NativeKind) -> Option<(NativeCallback, NativeKind)> {
    Some((native_cb as NativeCallback, kind))
}

const STRICT: ExecutionPolicy = ExecutionPolicy {
    compatibility_mode: CompatibilityMode::JdkOnly,
    real_jdk: true,
};
const COMPATIBLE: ExecutionPolicy = ExecutionPolicy {
    compatibility_mode: CompatibilityMode::Compatible,
    real_jdk: true,
};

/// Short label for assertion messages.
fn label(d: &DispatchDecision<'_>) -> String {
    match d {
        DispatchDecision::Bytecode(_) => "Bytecode".to_string(),
        DispatchDecision::NativeBridge(_) => "NativeBridge".to_string(),
        DispatchDecision::Intrinsic(_) => "Intrinsic".to_string(),
        DispatchDecision::Reject(v) => format!("Reject({})", v.kind()),
    }
}

// ---------------------------------------------------------------------------
// Step 1 — ACC_NATIVE binds to a bridge (or a reviewed intrinsic)
// ---------------------------------------------------------------------------

#[test]
fn acc_native_binds_to_a_registered_bridge_in_both_modes() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = native_method("socket0", "(ZZZZ)I");

    for policy in [COMPATIBLE, STRICT] {
        let d = resolve_dispatch(policy, class, &m, registered(NativeKind::Bridge));
        assert!(
            matches!(d, DispatchDecision::NativeBridge(_)),
            "an ACC_NATIVE method has no bytecode to run; it must bind to its \
             registered Bridge under {policy:?}, got {}",
            label(&d)
        );
    }
}

#[test]
fn acc_native_binds_to_a_registered_intrinsic_in_both_modes() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = native_method("currentTimeMillis", "()J");

    for policy in [COMPATIBLE, STRICT] {
        let d = resolve_dispatch(policy, class, &m, registered(NativeKind::Intrinsic));
        assert!(
            matches!(d, DispatchDecision::Intrinsic(_)),
            "got {} under {policy:?}",
            label(&d)
        );
    }
}

#[test]
fn strict_mode_rejects_invoking_a_synthetic_stub_native() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = native_method("fake", "(Ljava/lang/Object;)Z");

    let d = resolve_dispatch(STRICT, class, &m, registered(NativeKind::SyntheticStub));
    match d {
        DispatchDecision::Reject(JdkOnlyViolation::SyntheticNativeInvocation {
            class: c,
            method,
            descriptor,
            ..
        }) => {
            assert_eq!(c, OWNER);
            assert_eq!(method, "fake");
            assert_eq!(
                descriptor, "(Ljava/lang/Object;)Z",
                "the refusal must be identifiable down to the overload"
            );
        }
        other => panic!(
            "a SyntheticStub may not be invoked under --jdk-only (contract \
             §1.3); got {}",
            label(&other)
        ),
    }
}

#[test]
fn compatible_mode_still_dispatches_a_synthetic_stub_native() {
    // Wave 1 changes nothing in Compatible mode. Which native-shaped decision
    // the resolver returns for a stub is E's to choose; what this pins is that
    // it is NOT a policy refusal — a `Reject` here would break every existing
    // suite the moment the resolver went live.
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = native_method("fake", "(Ljava/lang/Object;)Z");

    let d = resolve_dispatch(COMPATIBLE, class, &m, registered(NativeKind::SyntheticStub));
    assert!(
        matches!(
            d,
            DispatchDecision::NativeBridge(_) | DispatchDecision::Intrinsic(_)
        ),
        "Compatible mode must keep today's behaviour byte-for-byte \
         (contract §10); got {}",
        label(&d)
    );
}

#[test]
fn an_unbound_acc_native_method_is_a_structured_missing_native() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = native_method("socket0", "(ZZZZ)I");

    let d = resolve_dispatch(STRICT, class, &m, None);
    match d {
        DispatchDecision::Reject(v @ JdkOnlyViolation::MissingNative { .. }) => {
            let summary = v.summary();
            assert!(summary.contains(OWNER), "{summary}");
            assert!(
                summary.contains("(ZZZZ)I"),
                "a missing-native report without the descriptor cannot be \
                 turned into a work item: {summary}"
            );
            assert_eq!(v.kind(), "missing-native");
        }
        other => panic!(
            "absence of a native is a structured MissingNative error, never a \
             stub and never a silent no-op (contract §1.5); got {}",
            label(&other)
        ),
    }
}

// ---------------------------------------------------------------------------
// Steps 2 and 3 — intrinsic may shadow bytecode; a bridge or stub may not
// ---------------------------------------------------------------------------

#[test]
fn concrete_bytecode_beats_a_registered_bridge() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = bytecode_method("compute", "()I");

    let d = resolve_dispatch(STRICT, class, &m, registered(NativeKind::Bridge));
    assert!(
        matches!(d, DispatchDecision::Bytecode(_)),
        "real class bytes are authoritative: a non-native method with a Code \
         attribute runs its own bytecode, and a registered Bridge does not \
         shadow it (contract §1.4 / §7 step 3); got {}",
        label(&d)
    );
}

#[test]
fn concrete_bytecode_beats_a_registered_synthetic_stub() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = bytecode_method("compute", "()I");

    let d = resolve_dispatch(STRICT, class, &m, registered(NativeKind::SyntheticStub));
    assert!(
        matches!(d, DispatchDecision::Bytecode(_)),
        "this is the failure mode the whole mode exists to remove — a fake \
         shadowing correct real bytecode; got {}",
        label(&d)
    );
}

#[test]
fn a_reviewed_intrinsic_may_shadow_concrete_bytecode() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = bytecode_method("hashCode", "()I");

    let d = resolve_dispatch(STRICT, class, &m, registered(NativeKind::Intrinsic));
    assert!(
        matches!(d, DispatchDecision::Intrinsic(_)),
        "an Intrinsic is the one reviewed exception: same answer, faster \
         (contract §1.4); got {}",
        label(&d)
    );
}

#[test]
fn concrete_bytecode_with_no_native_is_bytecode_in_both_modes() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = bytecode_method("compute", "()I");

    for policy in [COMPATIBLE, STRICT] {
        let d = resolve_dispatch(policy, class, &m, None);
        assert!(
            matches!(d, DispatchDecision::Bytecode(_)),
            "got {} under {policy:?}",
            label(&d)
        );
    }
}

#[test]
fn the_returned_bytecode_borrow_is_the_method_that_was_passed_in() {
    // `DispatchDecision::Bytecode(&'a Method)` hands the caller a borrow of
    // the *same* method, not a lookup of a same-named one. A resolver that
    // re-resolved by name here would reintroduce the super-call recursion bug
    // this repository has hit before (a virtual dispatch that re-enters the
    // subclass override).
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = bytecode_method("compute", "()I");

    match resolve_dispatch(STRICT, class, &m, None) {
        DispatchDecision::Bytecode(got) => {
            assert!(
                std::ptr::eq(got, &m),
                "the decision must borrow the method it was handed"
            );
        }
        other => panic!("got {}", label(&other)),
    }
}

// ---------------------------------------------------------------------------
// Step 4 — nothing to run
// ---------------------------------------------------------------------------

#[test]
fn a_method_with_neither_bytecode_nor_native_is_rejected() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);
    let m = abstract_method("compute", "()J");

    let d = resolve_dispatch(STRICT, class, &m, None);
    match d {
        DispatchDecision::Reject(v @ JdkOnlyViolation::MissingImplementation { .. }) => {
            assert_eq!(v.kind(), "missing-implementation");
            assert!(v.summary().contains("()J"), "{}", v.summary());
        }
        other => panic!(
            "a method with no Code attribute and no admissible native has \
             nothing to run; got {}",
            label(&other)
        ),
    }
}

// ---------------------------------------------------------------------------
// Compatible mode never refuses
// ---------------------------------------------------------------------------

#[test]
fn compatible_mode_produces_no_policy_refusal_for_any_runnable_shape() {
    // Wave 1's load-bearing promise: turning the resolver on must not change
    // what a default run does. Every shape that has *something* to run must
    // come back with something to run.
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let class = owner_class(&mut mgr);

    let cases: Vec<(&str, ClassFileMethod, Option<(NativeCallback, NativeKind)>)> = vec![
        (
            "native + bridge",
            native_method("a", "()V"),
            registered(NativeKind::Bridge),
        ),
        (
            "native + intrinsic",
            native_method("b", "()V"),
            registered(NativeKind::Intrinsic),
        ),
        (
            "native + stub",
            native_method("c", "()V"),
            registered(NativeKind::SyntheticStub),
        ),
        ("bytecode alone", bytecode_method("d", "()I"), None),
        (
            "bytecode + bridge",
            bytecode_method("e", "()I"),
            registered(NativeKind::Bridge),
        ),
        (
            "bytecode + stub",
            bytecode_method("f", "()I"),
            registered(NativeKind::SyntheticStub),
        ),
        (
            "bytecode + intrinsic",
            bytecode_method("g", "()I"),
            registered(NativeKind::Intrinsic),
        ),
    ];

    for (what, method, native) in cases {
        let d = resolve_dispatch(COMPATIBLE, class, &method, native);
        assert!(
            !matches!(d, DispatchDecision::Reject(_)),
            "Compatible mode has no policy refusals at all; {what} produced {}",
            label(&d)
        );
    }
}
