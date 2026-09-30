// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.4-B — `ClassManager::redefine_class` JEP 109 conformance.
//!
//! These tests pin down the contract of the JVMTI `RedefineClasses` /
//! `RetransformClasses` entry point as it lives in `class_manager`:
//!
//!   * Fresh `redefine_class` on existing class with structurally
//!     equivalent bytes succeeds and replaces method bodies.
//!   * Generation counter starts at 0, bumps on each successful
//!     redefine, never decrements.
//!   * Class identity is preserved (same `ClassId`, same name, same
//!     superclass id, same vtable layout).
//!   * `class_bytes_cache` is updated to the new bytes.
//!   * Constraint violations are rejected with
//!     `UnsupportedClassRedefinitionError`:
//!       - bytes too short / bad magic
//!       - new class name doesn't match existing name
//!       - superclass changed
//!       - direct interface list changed
//!       - field count or signature changed
//!       - method count or signature changed (add/remove forbidden)
//!   * `RedefineOptions::skip_structural_check = true` bypasses the
//!     structural check (but name match is still enforced).
//!
//! Fixtures: `Foo.v1.class` (foo()=1) and `Foo.v2.class` (foo()=2)
//! plus `Bar.class`, `Foo.extra_field.class`, `Foo.extra_method.class`,
//! and `Foo.impl_serializable.class`.

use cratonvm_classloading::{ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions};
use std::path::PathBuf;

const REDEFINE_FIXTURES: &[&str] = &[
    "Foo.v1.class",
    "Foo.v2.class",
    "Bar.class",
    "Foo.extra_field.class",
    "Foo.extra_method.class",
    "Foo.impl_serializable.class",
    "FooSubA.class",
    "FooSubB.class",
];

fn fixture_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("wp2_4b_redefine");
    p.push(name);
    p
}

fn load_fixture(name: &str) -> Option<Vec<u8>> {
    std::fs::read(fixture_path(name)).ok()
}

fn fresh_manager() -> ClassManager {
    ClassManager::new(&[], &[], &[])
}

#[test]
fn packaged_redefine_fixtures_are_present() {
    for fixture in REDEFINE_FIXTURES {
        let path = fixture_path(fixture);
        assert!(
            path.is_file(),
            "required redefine fixture must be packaged: {}",
            path.display()
        );
    }
}

// --------------------------------------------------------------------------
// 1. Happy path: round-trip redefine.
// --------------------------------------------------------------------------

#[test]
fn redefine_round_trip_replaces_method_body_and_bumps_generation() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return, // fixture not available
    };
    let v2 = match load_fixture("Foo.v2.class") {
        Some(b) => b,
        None => return,
    };

    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");

    // Generation starts at 0.
    assert_eq!(
        cm.class_redefine_generation(cid),
        0,
        "fresh class must report generation 0",
    );

    // Capture the v1 method body bytes for comparison after the swap.
    let v1_body = {
        let cls = cm.class_store.get(cid).unwrap();
        // Find the `foo` method (skip `<init>`).
        let m = cls
            .methods
            .iter()
            .find(|m| &*m.name == "foo")
            .expect("foo method present");
        m.code().expect("foo has Code attr").code.clone()
    };

    // Redefine with v2.
    cm.redefine_class(cid, v2.clone(), RedefineOptions::default())
        .expect("redefine to v2 ok");

    // Generation bumped to 1.
    assert_eq!(
        cm.class_redefine_generation(cid),
        1,
        "successful redefine must bump generation by 1",
    );

    // Method body changed.
    let v2_body = {
        let cls = cm.class_store.get(cid).unwrap();
        let m = cls
            .methods
            .iter()
            .find(|m| &*m.name == "foo")
            .expect("foo still present");
        m.code().expect("foo still has Code").code.clone()
    };
    assert_ne!(
        v1_body, v2_body,
        "method body bytes must differ after redefine",
    );

    // Class identity preserved: same id, same name.
    let cls = cm.class_store.get(cid).unwrap();
    assert_eq!(cls.id, cid);
    assert_eq!(&*cls.name, "Foo");

    // Class bytes cache updated to the new bytes.
    let cached = cm
        .class_bytes_cache
        .get(&cid)
        .expect("class_bytes_cache must hold latest bytes");
    // `class_bytes_cache` stores `SharedBytes`, which has no
    // `PartialEq<Vec<u8>>`; compare through its `Deref<Target = [u8]>`.
    assert_eq!(&cached[..], &v2[..]);

    // Vtable layout unchanged: still 1 virtual slot for foo()I.
    let entries = cm.vtable_descriptors_of(cid).expect("vtable present");
    let virtuals = entries.iter().filter(|e| e.is_some()).count();
    assert_eq!(virtuals, 1, "vtable virtual count must be unchanged");
    let foo_slot = entries
        .iter()
        .find_map(|e| e.as_ref())
        .expect("foo slot present");
    assert_eq!(&*foo_slot.method_name, "foo");
}

#[test]
fn redefine_back_to_v1_bumps_generation_again() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let v2 = match load_fixture("Foo.v2.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");
    cm.redefine_class(cid, v2, RedefineOptions::default())
        .expect("redefine v1->v2 ok");
    cm.redefine_class(cid, v1, RedefineOptions::default())
        .expect("redefine v2->v1 ok");
    assert_eq!(
        cm.class_redefine_generation(cid),
        2,
        "two successful redefines must bump generation to 2",
    );
}

#[test]
fn redefine_generation_handle_observes_increments() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let v2 = match load_fixture("Foo.v2.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");
    let handle = cm.class_redefine_generation_handle(cid);
    assert_eq!(handle.load(std::sync::atomic::Ordering::Acquire), 0);
    cm.redefine_class(cid, v2, RedefineOptions::default())
        .expect("redefine ok");
    assert_eq!(
        handle.load(std::sync::atomic::Ordering::Acquire),
        1,
        "shared handle must observe the bump",
    );
}

// --------------------------------------------------------------------------
// 2. Header-level rejections.
// --------------------------------------------------------------------------

#[test]
fn redefine_rejects_too_short() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("define ok");
    let err = cm
        .redefine_class(cid, vec![0xCA, 0xFE], RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("too short"),
        "expected 'too short' UnsupportedClassRedefinitionError, got: {msg}"
    );
}

#[test]
fn redefine_rejects_bad_magic() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("define ok");
    let mut bad = vec![0u8; 16];
    bad[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let err = cm
        .redefine_class(cid, bad, RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("bad magic"),
        "expected 'bad magic' UnsupportedClassRedefinitionError, got: {msg}"
    );
}

// --------------------------------------------------------------------------
// 3. Structural-equivalence rejections.
// --------------------------------------------------------------------------

#[test]
fn redefine_rejects_class_name_change() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let bar = match load_fixture("Bar.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("Foo define ok");
    // Bar.class has this_class = Bar, not Foo.
    let err = cm
        .redefine_class(cid, bar, RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("this_class"),
        "expected name-mismatch UnsupportedClassRedefinitionError, got: {msg}"
    );
    // Generation must NOT have bumped.
    assert_eq!(cm.class_redefine_generation(cid), 0);
}

#[test]
fn redefine_rejects_field_count_change() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let extra_field = match load_fixture("Foo.extra_field.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("Foo define ok");
    let err = cm
        .redefine_class(cid, extra_field, RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("field count"),
        "expected 'field count' UnsupportedClassRedefinitionError, got: {msg}"
    );
}

#[test]
fn redefine_rejects_method_count_change() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let extra_method = match load_fixture("Foo.extra_method.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("Foo define ok");
    let err = cm
        .redefine_class(cid, extra_method, RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("method count"),
        "expected 'method count' UnsupportedClassRedefinitionError, got: {msg}"
    );
}

#[test]
fn redefine_rejects_interface_change() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let with_iface = match load_fixture("Foo.impl_serializable.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("Foo define ok");
    let err = cm
        .redefine_class(cid, with_iface, RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("interface"),
        "expected interface-change UnsupportedClassRedefinitionError, got: {msg}"
    );
}

// --------------------------------------------------------------------------
// 4. skip_structural_check escape hatch.
// --------------------------------------------------------------------------

#[test]
fn redefine_skip_structural_check_still_enforces_name_match() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let bar = match load_fixture("Bar.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("Foo define ok");
    // Even with skip_structural_check, the name match is still
    // enforced — replacing Foo with Bar is never legal.
    let opts = RedefineOptions {
        skip_structural_check: true,
        ..Default::default()
    };
    let err = cm.redefine_class(cid, bar, opts).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("this_class"),
        "expected name-mismatch even with skip_structural_check, got: {msg}"
    );
}

#[test]
fn redefine_no_op_with_same_bytes_succeeds_and_bumps_generation() {
    // Passing the SAME bytes is a no-op redefinition and must succeed.
    // This is the canonical "instrumentation noop" path.
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("define ok");
    cm.redefine_class(cid, v1.clone(), RedefineOptions::default())
        .expect("identity redefine ok");
    assert_eq!(
        cm.class_redefine_generation(cid),
        1,
        "even an identity redefine bumps the generation",
    );
}

// --------------------------------------------------------------------------
// 5. Failure paths leave the manager unchanged.
// --------------------------------------------------------------------------

#[test]
fn rejected_redefine_does_not_swap_methods_or_bump_generation() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let bar = match load_fixture("Bar.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    let cid = cm
        .define_class_with_options("Foo", &v1, ClassLoaderId::Application, Default::default())
        .expect("define ok");
    let body_before = {
        let cls = cm.class_store.get(cid).unwrap();
        cls.methods
            .iter()
            .find(|m| &*m.name == "foo")
            .and_then(|m| m.code())
            .map(|c| c.code.clone())
            .unwrap_or_default()
    };

    let _ = cm.redefine_class(cid, bar, RedefineOptions::default()); // expected to fail

    let body_after = {
        let cls = cm.class_store.get(cid).unwrap();
        cls.methods
            .iter()
            .find(|m| &*m.name == "foo")
            .and_then(|m| m.code())
            .map(|c| c.code.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        body_before, body_after,
        "rejected redefine must not mutate methods"
    );
    assert_eq!(
        cm.class_redefine_generation(cid),
        0,
        "rejected redefine must not bump generation"
    );
}

// --------------------------------------------------------------------------
// 6. Unknown class id rejected.
// --------------------------------------------------------------------------

#[test]
fn redefine_unknown_class_id_rejected() {
    let v1 = match load_fixture("Foo.v1.class") {
        Some(b) => b,
        None => return,
    };
    let mut cm = fresh_manager();
    // Allocate an id that's beyond the store's range.
    let bogus = cratonvm_classloading::ClassId::new(99_999);
    let err = cm
        .redefine_class(bogus, v1, RedefineOptions::default())
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UnsupportedClassRedefinition") && msg.contains("not loaded"),
        "expected 'not loaded' UnsupportedClassRedefinitionError, got: {msg}"
    );
}

// --------------------------------------------------------------------------
// Redefine must not leave a stale dispatch snapshot in the cached vtable
// slot descriptors of ALREADY-LINKED subclasses.
//
// `vtable_descriptors` is the seed a newly linked class copies its inherited
// slots from (`build_vtable_descriptors_with_overrides`). Refreshing only the
// redefined class's own vec left every subclass linked BEFORE the redefine
// naming the OLD code, so a class linked AFTER the redefine inherited the
// pre-redefine bytecode with no generation check anywhere able to notice
// (the descriptor is `resolved`, and its `declaring_class_id` is the
// redefined class, so every redefine guard reads "current").
//
// Real-world shape: Mockito's inline mock maker retransforms
// `java.io.OutputStream`, then generates a mock subclass of the
// already-linked `jakarta.servlet.ServletOutputStream`. The mock's
// `write([BII)V` vtable slot pointed at the un-woven body, so the mock
// silently stopped intercepting once the call site reached the vtable fast
// path.
// --------------------------------------------------------------------------

#[test]
fn redefine_refreshes_dispatch_snapshot_in_already_linked_subclasses() {
    let (v1, v2, sub_a, sub_b) = match (
        load_fixture("Foo.v1.class"),
        load_fixture("Foo.v2.class"),
        load_fixture("FooSubA.class"),
        load_fixture("FooSubB.class"),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return, // fixtures not available
    };

    let mut cm = fresh_manager();
    let foo_id = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("Foo v1 define ok");
    // FooSubA is linked BEFORE the redefine, so its cached descriptor vec
    // captures Foo's v1 dispatch snapshot for the inherited `foo()I` slot.
    let sub_a_id = cm
        .define_class_with_options(
            "FooSubA",
            &sub_a,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("FooSubA define ok");

    let inherited_foo_code = |cm: &ClassManager, id| -> Vec<u8> {
        cm.vtable_descriptors_of(id)
            .expect("vtable present")
            .iter()
            .flatten()
            .find(|d| &*d.method_name == "foo" && &*d.descriptor == "()I")
            .and_then(|d| d.dispatch.as_ref())
            .expect("foo slot carries a dispatch snapshot")
            .code
            .to_vec()
    };

    let v1_code = inherited_foo_code(&cm, foo_id);
    assert_eq!(
        inherited_foo_code(&cm, sub_a_id),
        v1_code,
        "subclass must inherit the superclass snapshot at link time",
    );

    cm.redefine_class(foo_id, v2, RedefineOptions::default())
        .expect("redefine to v2 ok");

    let v2_code = inherited_foo_code(&cm, foo_id);
    assert_ne!(v1_code, v2_code, "fixtures must differ in `foo` body");

    // The already-linked subclass's cached descriptor must have been
    // refreshed in place.
    assert_eq!(
        inherited_foo_code(&cm, sub_a_id),
        v2_code,
        "already-linked subclass kept a stale pre-redefine dispatch snapshot",
    );

    // ...and a class linked AFTER the redefine, which seeds from that
    // subclass, must therefore see the redefined body too.
    let sub_b_id = cm
        .define_class_with_options(
            "FooSubB",
            &sub_b,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("FooSubB define ok");
    assert_eq!(
        inherited_foo_code(&cm, sub_b_id),
        v2_code,
        "class linked after the redefine inherited the pre-redefine body",
    );
}
