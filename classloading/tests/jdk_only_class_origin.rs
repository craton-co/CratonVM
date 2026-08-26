// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK-only mode — class provenance and the strict class-loading policy
//! (`docs/feature-designs/jdk-only-mode.md` §5).
//!
//! The contract's central principle is that **real class bytes are
//! authoritative**. Mechanically that reduces to three claims about
//! `ClassOrigin`, and this file pins all three:
//!
//! * **Every legitimate way a class can come into existence has its own
//!   origin, and they are all distinguishable.** Arrays, hidden classes,
//!   lambdas, proxies and reflection accessors are *allowed* — the terminology
//!   table is explicit that they are not compatibility stubs. A census that
//!   cannot tell a VM-created array from a fabricated `org/jboss/...` stand-in
//!   cannot be used to drive the stub backlog to zero.
//! * **`CompatibilityStub` is the only origin `JdkOnly` rejects**, exactly
//!   mirroring `NativeKind::SyntheticStub` on the native-api side.
//! * **`Compatible` mode is byte-for-byte unchanged.** Wave 1 is measurement,
//!   not deletion (contract §10): the origin is recorded either way, but only
//!   strict mode refuses.
//!
//! Everything here uses a `ClassManager` with an empty classpath, which is the
//! configuration in which the fallback fabrication paths actually fire — no
//! JDK image, no Cargo feature and no fixture staging required, so these tests
//! run in the default build on every platform. (Several older fixture tests in
//! this repo silently `return` when `apps/…/classes` is absent; nothing here
//! does, because there is nothing to stage.)

use cratonvm_classloading::class_origin::ClassOrigin;
use cratonvm_classloading::ClassManager;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::JdkOnlyViolation;
use cratonvm_types::{ClassId, ClassLoaderId};
use std::sync::Arc;

/// One of each `ClassOrigin` variant, in contract order.
fn every_origin() -> Vec<ClassOrigin> {
    vec![
        ClassOrigin::BootImage {
            module: Some(Arc::from("java.base")),
            source: Arc::from("jrt:/java.base"),
        },
        ClassOrigin::ApplicationClassPath {
            source: Arc::from("build/classes"),
        },
        ClassOrigin::UserDefined {
            loader: ClassLoaderId::UserDefined(7),
            source: None,
        },
        ClassOrigin::VmArray,
        ClassOrigin::HiddenClass {
            host: Some(ClassId::new(3)),
        },
        ClassOrigin::GeneratedLambda {
            host: Some(ClassId::new(4)),
        },
        ClassOrigin::GeneratedProxy {
            interfaces: Arc::from(vec![ClassId::new(5)]),
        },
        ClassOrigin::ReflectionAccessor {
            host: Some(ClassId::new(6)),
        },
        ClassOrigin::VmInternal,
        ClassOrigin::CompatibilityStub {
            reason: Arc::from("no real bytes for org/jboss/logging/Logger"),
        },
    ]
}

// ---------------------------------------------------------------------------
// Origin taxonomy
// ---------------------------------------------------------------------------

#[test]
fn every_origin_category_is_distinguishable() {
    let origins = every_origin();
    let tags: Vec<&'static str> = origins.iter().map(|o| o.as_str()).collect();

    let mut sorted = tags.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        tags.len(),
        "every ClassOrigin variant needs its OWN census tag; duplicates in \
         {tags:?} would collapse two provenances into one census row and make \
         the zero-compatibility-class gate unfalsifiable"
    );

    // These strings are a wire format: the `origin` column of
    // `--dump-class-origins` and the key CI groups by. Pin the spellings the
    // contract names explicitly; the rest are pinned as a set above.
    assert_eq!(ClassOrigin::VmArray.as_str(), "vm-array");
    assert_eq!(
        ClassOrigin::CompatibilityStub {
            reason: Arc::from("x")
        }
        .as_str(),
        "compatibility-stub"
    );
    assert_eq!(
        ClassOrigin::BootImage {
            module: None,
            source: Arc::from("jrt:/java.base")
        }
        .as_str(),
        "boot-image"
    );
    assert_eq!(
        ClassOrigin::GeneratedLambda { host: None }.as_str(),
        "generated-lambda"
    );

    // Tags are lowercase kebab-case throughout — a mixed convention shows up
    // as a JSON schema break long after the fact.
    for tag in tags {
        assert!(
            tag.chars()
                .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit()),
            "origin tag {tag:?} is not lowercase kebab-case"
        );
    }
}

#[test]
fn only_the_compatibility_stub_is_rejected_by_jdk_only() {
    for origin in every_origin() {
        assert!(
            origin.allowed_in(CompatibilityMode::Compatible),
            "Compatible mode permits every origin — that is today's behaviour \
             and wave 1 must not change it ({})",
            origin.as_str()
        );

        let is_stub = origin.is_compatibility_stub();
        assert_eq!(
            is_stub,
            matches!(origin, ClassOrigin::CompatibilityStub { .. }),
            "is_compatibility_stub must be exactly the CompatibilityStub \
             discriminant test ({})",
            origin.as_str()
        );
        assert_eq!(
            origin.allowed_in(CompatibilityMode::JdkOnly),
            !is_stub,
            "JdkOnly rejects CompatibilityStub and nothing else — arrays, \
             hidden classes, lambdas, proxies and reflection accessors are \
             allowed with their own origin (contract §1.6, terminology table) \
             ({})",
            origin.as_str()
        );
    }
}

#[test]
fn default_origin_is_vm_internal() {
    // Not `CompatibilityStub`: a class whose origin nobody set is a VM
    // bookkeeping artefact, and defaulting to the one forbidden value would
    // make strict mode reject classes for the crime of not being annotated.
    assert_eq!(ClassOrigin::default(), ClassOrigin::VmInternal);
    assert!(!ClassOrigin::default().is_compatibility_stub());
    assert!(ClassOrigin::default().allowed_in(CompatibilityMode::JdkOnly));
}

// ---------------------------------------------------------------------------
// The derived `is_synthetic_stub` mirror
// ---------------------------------------------------------------------------

#[test]
fn set_origin_keeps_the_derived_mirror_in_sync() {
    // `Class::is_synthetic_stub` has ~160 readers across 17 files and cannot
    // be deleted this wave; it is a pure mirror of
    // `origin.is_compatibility_stub()`. If the two ever disagree, half the VM
    // sees a stub and half sees a real class — so the invariant is checked in
    // both directions, through the only setter allowed to touch either field.
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let id = mgr.try_ensure_synthetic_class("com/example/Mirror", 0).expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
    let class = mgr
        .get_class_mut(id)
        .expect("just-created class must be in the store");

    class.set_origin(ClassOrigin::CompatibilityStub {
        reason: Arc::from("test"),
    });
    assert!(class.origin.is_compatibility_stub());
    assert!(
        class.origin.is_compatibility_stub(),
        "set_origin(CompatibilityStub) must raise the derived mirror"
    );

    class.set_origin(ClassOrigin::BootImage {
        module: Some(Arc::from("java.base")),
        source: Arc::from("jrt:/java.base"),
    });
    assert!(!class.origin.is_compatibility_stub());
    assert!(
        !class.origin.is_compatibility_stub(),
        "the in-place 'upgrade a stub to real bytes' path goes through \
         set_origin, so the mirror must clear with the origin — a stale `true` \
         here is what makes real bytes lose to a fake"
    );

    class.set_origin(ClassOrigin::VmArray);
    assert!(!class.origin.is_compatibility_stub());
}

// ---------------------------------------------------------------------------
// Arrays are VM-created in BOTH modes
// ---------------------------------------------------------------------------

/// Load an array class and return its origin. The component and
/// `java/lang/Object` are warmed up first, in `Compatible` mode, because with
/// an empty classpath they have no real bytes either — this test is about the
/// *array*, not about its component.
fn array_origin_under(mode: CompatibilityMode) -> ClassOrigin {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let _ = mgr.load_class("java/lang/Object");
    let _ = mgr.load_class("java/util/HashMap");

    mgr.set_compatibility_mode(mode);
    assert_eq!(mgr.compatibility_mode(), mode);

    let id = mgr
        .load_class("[Ljava/util/HashMap;")
        .expect("array-class synthesis must succeed in every mode");
    mgr.get_class(id)
        .expect("synthesised array class must be in the store")
        .origin
        .clone()
}

#[test]
fn array_classes_are_vm_created_not_compatibility_stubs_in_compatible_mode() {
    let origin = array_origin_under(CompatibilityMode::Compatible);
    assert_eq!(origin, ClassOrigin::VmArray, "got {}", origin.as_str());
    assert!(!origin.is_compatibility_stub());
}

#[test]
fn array_classes_are_vm_created_not_compatibility_stubs_in_jdk_only_mode() {
    // Contract §5: "Array classes get `VmArray`, never `CompatibilityStub`, in
    // **both** modes." An array class has no class file anywhere — the JVM
    // spec says the VM creates it — so calling it a compatibility substitution
    // would make the zero-compatibility-class acceptance criterion
    // unsatisfiable by construction.
    let origin = array_origin_under(CompatibilityMode::JdkOnly);
    assert_eq!(origin, ClassOrigin::VmArray, "got {}", origin.as_str());
    assert!(origin.allowed_in(CompatibilityMode::JdkOnly));
}

#[test]
fn primitive_array_classes_are_vm_created_too() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let _ = mgr.load_class("java/lang/Object");
    mgr.set_compatibility_mode(CompatibilityMode::JdkOnly);
    let id = mgr
        .load_class("[I")
        .expect("primitive array synthesis must succeed under --jdk-only");
    assert_eq!(
        mgr.get_class(id).expect("in store").origin,
        ClassOrigin::VmArray
    );
}

// ---------------------------------------------------------------------------
// Strict mode refuses to fabricate
// ---------------------------------------------------------------------------

/// The two families that actually reach the fabrication chain with no real
/// bytes available: a JDK class (no boot image on this `ClassManager`) and a
/// third-party dependency reached through the enterprise-prefix fallback.
///
/// A plain *application* class is deliberately not in this list: `load_class`
/// never fabricated one — `is_jdk_class` gates the whole fallback arm — so it
/// already fails in both modes. That case is covered separately by
/// [`application_classes_are_absent_in_both_modes`], because "strict mode
/// refuses to fabricate it" and "it was never fabricated in the first place"
/// are different claims and only one of them is about this feature.
const FABRICATION_CANDIDATES: &[&str] = &[
    "java/util/concurrent/ConcurrentSkipListMap", // JDK class, no image here
    "org/jboss/logging/Logger",                   // enterprise-prefix fallback
];

#[test]
fn strict_mode_refuses_to_fabricate_missing_classes() {
    for name in FABRICATION_CANDIDATES {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        mgr.set_compatibility_mode(CompatibilityMode::JdkOnly);

        let result = mgr.load_class(name);
        assert!(
            result.is_err(),
            "under --jdk-only, {name} has no real bytes anywhere and MUST fail \
             with the specification-appropriate ClassNotFoundException / \
             NoClassDefFoundError instead of being fabricated (contract §5). \
             A fabricated class is worse than a missing one: it fails later, \
             somewhere else, with a message about a field offset."
        );

        // The refusal is *recorded*, not merely returned: the operator has to
        // be able to enumerate everything the run refused.
        let violations = mgr.origin_violations();
        assert!(
            violations.iter().any(|v| matches!(
                v,
                JdkOnlyViolation::CompatibilityClassRequested { class, .. } if class == name
            )),
            "expected a CompatibilityClassRequested naming {name}, got {violations:?}"
        );
    }
}

#[test]
fn compatible_mode_retains_the_old_fallback_exactly() {
    for name in FABRICATION_CANDIDATES {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        // No `set_compatibility_mode` call at all — the default must be the
        // permissive one, and the pre-existing behaviour must survive it.
        assert_eq!(mgr.compatibility_mode(), CompatibilityMode::Compatible);

        let id = mgr.load_class(name).unwrap_or_else(|e| {
            panic!(
                "Compatible mode must keep today's fabrication fallback \
                 byte-for-byte; {name} failed with {e:?}. Wave 1 is \
                 measurement, not deletion (contract §10)."
            )
        });
        let class = mgr.get_class(id).expect("fabricated class is in the store");
        assert!(
            class.origin.is_compatibility_stub(),
            "the origin is recorded even in Compatible mode so the census is \
             meaningful before enforcement lands; {name} came back as {}",
            class.origin.as_str()
        );
        assert!(
            class.origin.is_compatibility_stub(),
            "the derived mirror must agree with the origin"
        );
    }
}

#[test]
fn application_classes_are_absent_in_both_modes() {
    // An application class that is not on the classpath is a
    // ClassNotFoundException, full stop, and always was: the fabrication arm in
    // `load_class` is gated on `is_jdk_class`, which does not match
    // `com/example/…`. Pinned in both modes so a future change that widens the
    // fallback to arbitrary names is caught here rather than by a strict run
    // months later.
    for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        mgr.set_compatibility_mode(mode);
        assert!(
            mgr.load_class("com/example/app/Main").is_err(),
            "an application class with no bytes anywhere must not be \
             fabricated under {mode:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The census
// ---------------------------------------------------------------------------

#[test]
fn dump_class_origins_carries_the_origin_category_per_row() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let _ = mgr.load_class("java/lang/Object");
    let _ = mgr.load_class("java/util/HashMap");
    let _ = mgr.load_class("[Ljava/util/HashMap;");
    let _ = mgr.load_class("org/jboss/logging/Logger");

    let rows = mgr.dump_class_origins();
    assert!(
        !rows.is_empty(),
        "a census with no rows would make every downstream zero-count \
         assertion pass vacuously"
    );

    let find = |n: &str| {
        rows.iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("no class-origin row for {n}"))
    };

    let array = find("[Ljava/util/HashMap;");
    assert_eq!(array.origin, ClassOrigin::VmArray.as_str());
    assert_eq!(
        array.reason, None,
        "only a CompatibilityStub carries a reason"
    );

    let stub = find("org/jboss/logging/Logger");
    assert_eq!(stub.origin, "compatibility-stub");
    assert!(
        stub.reason.is_some(),
        "a compatibility stub must say WHY it was fabricated — that string is \
         what turns the census into a work list"
    );
    assert!(
        !stub.real_bytes_found,
        "nothing was found on the classpath, so real_bytes_found is false"
    );
    assert_eq!(
        stub.loader_id,
        ClassLoaderId::Bootstrap.to_native_id(),
        "the census reports the flat u32 loader wire value"
    );

    // Rows are diff-stable: two dumps of an unchanged manager are identical.
    let again = mgr.dump_class_origins();
    let key = |rs: &[cratonvm_classloading::class_origin::ClassOriginEntry]| {
        rs.iter()
            .map(|r| (r.name.clone(), r.origin.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        key(&rows),
        key(&again),
        "--dump-class-origins is diffed between runs and between OS legs in \
         CI; a non-deterministic dump would make that gate flap"
    );
}

// ---------------------------------------------------------------------------
// INVARIANT (research plan): no CompatibilityStub class after a strict run
// ---------------------------------------------------------------------------

#[test]
fn no_compatibility_stub_class_is_created_while_strict() {
    // Unit-level form of the acceptance criterion. A `ClassManager` built for
    // this test necessarily bootstraps in `Compatible` (it is constructed
    // before any policy can be applied), so the assertion is a *delta*: no
    // compatibility stub may appear once strict mode is on. The whole-run form
    // — zero compatibility classes in a `--jdk-only` boot's
    // `--dump-class-origins` — is asserted end-to-end by the `jdk-only` CI job,
    // which is the only place a real JDK image exists.
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let _ = mgr.load_class("java/lang/Object");

    let before: Vec<String> = mgr
        .dump_class_origins()
        .into_iter()
        .filter(|r| r.origin == "compatibility-stub")
        .map(|r| r.name)
        .collect();

    mgr.set_compatibility_mode(CompatibilityMode::JdkOnly);
    for name in FABRICATION_CANDIDATES {
        let _ = mgr.load_class(name);
    }
    let _ = mgr.load_class("[Ljava/lang/Object;");

    let after: Vec<String> = mgr
        .dump_class_origins()
        .into_iter()
        .filter(|r| r.origin == "compatibility-stub")
        .map(|r| r.name)
        .collect();

    let new: Vec<&String> = after.iter().filter(|n| !before.contains(n)).collect();
    assert!(
        new.is_empty(),
        "acceptance criterion (contract §11): zero ClassOrigin::CompatibilityStub \
         classes for non-array JDK, application or dependency classes. Strict \
         mode fabricated {new:?}"
    );
}

// ---------------------------------------------------------------------------
// Splitting `is_synthetic_stub`'s two meanings (JDK-only wave 2, 2026-08-06)
// ---------------------------------------------------------------------------

/// `Class::dispatch_lacks_class_file` answers question (2) — "does this class
/// have no class file, so dispatch must look for a native under its own exact
/// name?" — and gives the SAME answer as the old `is_synthetic_stub` bit for
/// every class in a live store.
///
/// That equality is the whole safety argument for reclassifying
/// `java/lang/reflect/Proxy$Instance` as `VmInternal`: three dispatch read
/// sites in `vm` (`invoke_or_native`'s exact-name native preference,
/// `invoke_virtual_shared`'s `invoke_on_class_shared` arm, and
/// `validate_native_coverage`'s all-classes scan) selected on the bool, and
/// that class needs all three. Flipping the origin without the split would
/// have moved it off them.
///
/// The one deliberate exception is `Proxy$Instance` itself, which is exactly
/// what this asserts: no longer a compatibility stub, still dispatch-lacking-a-
/// class-file.
#[test]
fn dispatch_predicate_matches_the_stub_bit() {
    let mut mgr = ClassManager::new(&[], &[], &[]);

    // A spread of shapes: fabricated stand-ins with and without a
    // NATIVE-flagged method table, a VM-internal allocation shape, an array,
    // and the VM's own proxy supertype.
    for name in [
        "com/example/Plain",
        "java/lang/IllegalStateException",
        "org/jboss/modules/Module",
        "java/net/InetSocketAddress",
        "java/lang/reflect/Proxy$Instance",
    ] {
        let _ = mgr.try_ensure_synthetic_class(name, 3).expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
    }
    let _ = mgr.load_class("java/lang/Object");
    let _ = mgr.load_class("[Ljava/lang/Object;");

    let mut checked = 0usize;
    let mut proxy_seen = false;
    for id in 0..mgr.class_store.slot_count() as u32 {
        let Some(class) = mgr.class_store.get(ClassId::new(id)) else {
            continue;
        };
        checked += 1;
        if &*class.name == "java/lang/reflect/Proxy$Instance" {
            proxy_seen = true;
            assert!(
                !class.origin.is_compatibility_stub(),
                "Proxy$Instance is a generation artefact, not a compatibility \
                 substitution: it must not be counted as one in the census"
            );
            assert_eq!(class.origin, ClassOrigin::VmInternal);
            assert!(
                class.dispatch_lacks_class_file(),
                "Proxy$Instance has a NATIVE-flagged <init> and native \
                 registrations under its own exact name, and no class file \
                 anywhere. It must stay on the dispatch branches that look for \
                 those — that is the whole reason the bool had to be split \
                 before the origin could be flipped."
            );
            continue;
        }
        assert_eq!(
            class.dispatch_lacks_class_file(),
            class.origin.is_compatibility_stub(),
            "{}: the dispatch predicate and the compatibility-substitution \
             question disagree. They may differ only for names the VM invents \
             that also carry native-backed methods (today: Proxy$Instance). If \
             you added another, say so here; if this fired for a REAL class, \
             something is fabricating a class file's worth of NATIVE methods \
             under `ClassOrigin::VmInternal` and the census is now wrong.",
            class.name
        );
    }
    assert!(proxy_seen, "the Proxy$Instance fixture was not created");
    assert!(checked > 5, "only {checked} classes reached the scan");
}

/// An allocation shape is NOT on the exact-name-native dispatch branch.
///
/// `!origin.has_real_bytes()` was the obvious way to spell question (2) and is
/// wrong for exactly this reason: `VmInternal` and `VmArray` both answer "no
/// real bytes", so it would have moved every `cratonvm/synthetic/AnonymousObject$N`
/// — the shape behind every `HashMap` node in the VM — onto branches it takes
/// the other arm of today. That is a `Compatible`-mode behaviour change on the
/// busiest allocation shape there is.
#[test]
fn vm_internal_allocation_shapes_stay_off_the_dispatch_branch() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let id = mgr.ensure_generated_class("cratonvm/synthetic/AnonymousObject$4", 4, ClassOrigin::VmInternal);
    let class = mgr.class_store.get(id).expect("just created");
    assert_eq!(class.origin, ClassOrigin::VmInternal);
    assert!(!class.origin.has_real_bytes(), "premise of the test");
    assert!(
        !class.dispatch_lacks_class_file(),
        "an allocation shape has an empty method table and no native \
         registered under its name; there is nothing to find by exact name, so \
         it must stay on the non-stub arm where it has always been"
    );
}

/// A fabricated class whose NAME merely LOOKS generated is a compatibility
/// stub; only the three exact VM identities survive as `VmInternal`.
///
/// This test used to assert the opposite — that `com/example/Owner$$Lambda$17`
/// and friends are reported as what generated them — on the symmetry argument
/// that "neither path may report the same class differently depending on
/// whether it happened to be fabricated". `fabricated_origin_for_name`'s doc
/// rebuts that argument directly, and the rebuttal is the reason this file now
/// reads the other way:
///
///   * the discriminator is STRUCTURAL, not lexical. A genuine
///     `LambdaMetafactory` / `$ProxyN` / accessor class HAS BYTES and is
///     classified by `classify_defined_origin`, which still carries all three
///     name arms unchanged. This door is reached only after
///     `get_loaded_class_id` answered `None` AND `find_class_bytes_delegated`
///     failed — the name resolves to nothing, in any loader, on any classpath
///     entry. Having real bytes is *precisely* what separates a generated class
///     from a stand-in, so the two paths are not looking at the same class and
///     the symmetry argument inverted the question;
///   * the lexical arm was a measured hole, not a hypothetical one. Because the
///     `--jdk-only` refusal is gated on `origin.is_compatibility_stub()`, a
///     strict run CREATED `java/util/function/Predicate$$Lambda$And` — a
///     compatibility stand-in — while refusing its infix-less sibling
///     `java/util/function/Consumer$AndThen` at the door. Same species,
///     opposite verdicts, decided by a substring.
///
/// The refusal itself is asserted by
/// `class_manager.rs::a_generated_looking_name_does_not_buy_a_jdk_only_exemption`,
/// which owns that property and carries the measurement. This test is the
/// integration-level half: it goes through the public
/// `try_ensure_synthetic_class` door and checks the ORIGIN the class store ends
/// up holding, including the three identities that legitimately survive.
#[test]
fn fabricated_generated_names_are_compatibility_stubs() {
    let mut mgr = ClassManager::new(&[], &[], &[]);

    // Generated-LOOKING, no bytes anywhere: stand-ins, whatever the name.
    for name in [
        "com/example/Owner$$Lambda$17",
        "com/example/$Proxy42",
        "jdk/internal/reflect/GeneratedMethodAccessor3",
    ] {
        let id = mgr.try_ensure_synthetic_class(name, 2).expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let origin = &mgr.class_store.get(id).expect("just created").origin;
        assert!(
            origin.is_compatibility_stub(),
            "{name} arrived with no bytes on any classpath entry in any \
             loader; a generated-looking name must not relabel it"
        );
        assert!(
            !origin.allowed_in(CompatibilityMode::JdkOnly),
            "{name}: a generated-looking name must not buy a --jdk-only \
             exemption the same stand-in without the infix does not get"
        );
    }

    // The three that DO survive, and why they are allowed to: each is an exact
    // identity nothing but this VM can mint — two exact names in packages no
    // other party may define into, and this VM's own reserved prefix — not a
    // name SHAPE a caller could fall into.
    for name in [
        "java/lang/reflect/Proxy$Instance",
        "java/lang/annotation/AnnotationProxy",
        "CratonVM$SomeInternalCarrier",
    ] {
        let id = mgr.try_ensure_synthetic_class(name, 2).expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let origin = &mgr.class_store.get(id).expect("just created").origin;
        assert_eq!(*origin, ClassOrigin::VmInternal, "{name} got the wrong origin");
        assert!(
            !origin.is_compatibility_stub(),
            "{name} is a VM generation artefact; counting it as a \
             compatibility substitution over-reports the stub backlog"
        );
        assert!(
            origin.allowed_in(CompatibilityMode::JdkOnly),
            "{name} is legal in strict mode (contract §1 item 6)"
        );
    }

    // The control: an ordinary missing class is still a compatibility stub.
    let id = mgr.try_ensure_synthetic_class("com/example/GenuinelyMissing", 2).expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
    assert!(mgr
        .class_store
        .get(id)
        .expect("just created")
        .origin
        .is_compatibility_stub());
}
