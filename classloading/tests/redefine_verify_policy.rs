// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A redefine must not be judged by a STRICTER verification policy than the
//! definition it replaces.
//!
//! `define_class_with_options` defers the Pass-3 *type-state* verdict for any
//! class whose defining loader is `UserDefined` while `loader_aware_resolution()`
//! is on (the default): that hierarchy adapter cannot preserve both loader
//! identities through every pre-definition edge and produces false
//! areturn/checkcast rejections for otherwise valid forked bytecode. It enforces
//! the hierarchy-independent structural half (JVMS §4.9.1) and moves on.
//!
//! `redefine_class` used to run the FULL verifier regardless. So every
//! application class in a Spring / WildFly / H2 / Elasticsearch run — all of
//! them defined by user loaders — was excused from Pass 3 when it loaded and
//! then held to it the moment a JVMTI agent retransformed it. Mockito's inline
//! mock maker retransforms every class it mocks, so that was the one place the
//! false rejections the deferral exists to avoid could actually surface: the
//! class loads, the mock is created, and `retransformClasses0` logs an
//! `UnsupportedClassRedefinitionError` naming a verifier complaint about
//! bytecode the VM had already accepted.
//!
//! Fixtures are shared with `wp2_4b_redefine.rs`.

use cratonvm_classloading::{ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions};
use std::path::PathBuf;

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

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Byte offset of the named method's `Code` attribute `max_stack` field.
/// `max_locals` is the two bytes immediately after it.
fn code_header_offset(bytes: &[u8], want_method: &str) -> usize {
    let mut pos = 8usize;
    let cp_count = u16_at(bytes, pos) as usize;
    pos += 2;
    let mut utf8: std::collections::HashMap<u16, String> = std::collections::HashMap::new();
    let mut idx = 1usize;
    while idx < cp_count {
        let tag = bytes[pos];
        pos += 1;
        let slots = match tag {
            1 => {
                let len = u16_at(bytes, pos) as usize;
                pos += 2;
                utf8.insert(
                    idx as u16,
                    String::from_utf8_lossy(&bytes[pos..pos + len]).into_owned(),
                );
                pos += len;
                1
            }
            5 | 6 => {
                pos += 8;
                2
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                pos += 4;
                1
            }
            15 => {
                pos += 3;
                1
            }
            7 | 8 | 16 | 19 | 20 => {
                pos += 2;
                1
            }
            other => panic!("unexpected constant pool tag {other}"),
        };
        idx += slots;
    }
    pos += 6; // access_flags, this_class, super_class
    let ifaces = u16_at(bytes, pos) as usize;
    pos += 2 + 2 * ifaces;

    let field_count = u16_at(bytes, pos) as usize;
    pos += 2;
    for _ in 0..field_count {
        pos += 6;
        let attrs = u16_at(bytes, pos) as usize;
        pos += 2;
        for _ in 0..attrs {
            pos += 2;
            let len = u32_at(bytes, pos) as usize;
            pos += 4 + len;
        }
    }

    let method_count = u16_at(bytes, pos) as usize;
    pos += 2;
    for _ in 0..method_count {
        pos += 2; // access_flags
        let name = utf8[&u16_at(bytes, pos)].clone();
        pos += 4; // name_index + descriptor_index
        let attrs = u16_at(bytes, pos) as usize;
        pos += 2;
        for _ in 0..attrs {
            let attr_name = utf8[&u16_at(bytes, pos)].clone();
            pos += 2;
            let len = u32_at(bytes, pos) as usize;
            pos += 4;
            if attr_name == "Code" && name == want_method {
                return pos;
            }
            pos += len;
        }
    }
    panic!("no Code attribute for method `{want_method}`");
}

/// Bytes that fail **only** Pass 3.
///
/// An under-declared `max_stack` is the cleanest such defect:
/// `verify_method_structural` checks decode, branch and handler bounds,
/// `max_locals` and local operand indices, and never looks at `max_stack`. So
/// these bytes clear the structural half and fail the type-state half — exactly
/// the split the deferral is about. `foo()` is `iconst_N; ireturn`, one stack
/// slot, so declaring zero makes the very first push exceed the limit.
fn with_zero_max_stack(bytes: &[u8], method: &str) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let off = code_header_offset(bytes, method);
    out[off..off + 2].copy_from_slice(&0u16.to_be_bytes());
    out
}

/// Bytes that fail the **structural** scan: `max_locals = 0` cannot hold `this`
/// for an instance method, a JVMS §4.9.1 static constraint.
fn with_zero_max_locals(bytes: &[u8], method: &str) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let off = code_header_offset(bytes, method);
    out[off + 2..off + 4].copy_from_slice(&0u16.to_be_bytes());
    out
}

#[test]
fn redefine_of_a_user_loader_class_is_not_held_to_a_deferred_pass3() {
    let (v1, v2) = match (load_fixture("Foo.v1.class"), load_fixture("Foo.v2.class")) {
        (Some(a), Some(b)) => (a, b),
        _ => return, // fixtures not packaged
    };
    let v2_bad_stack = with_zero_max_stack(&v2, "foo");

    let mut cm = ClassManager::new(&[], &[], &[]);
    let cid = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::UserDefined(7),
            DefineClassOptions::default(),
        )
        .expect("v1 defines under a user loader (Pass 3 deferred)");

    cm.redefine_class(cid, v2_bad_stack, RedefineOptions::default())
        .expect(
            "a class whose definition was excused from Pass 3 must not have Pass 3 \
             enforced against its redefine",
        );
    assert_eq!(cm.class_redefine_generation(cid), 1);
}

#[test]
fn redefine_of_an_application_loader_class_still_runs_pass3() {
    // The control. The deferral is keyed on the defining loader, so a class the
    // VM DID fully verify at define time must still be fully verified on
    // redefine — otherwise the test above would read as "redefine verification
    // works" while it had in fact been switched off everywhere.
    let (v1, v2) = match (load_fixture("Foo.v1.class"), load_fixture("Foo.v2.class")) {
        (Some(a), Some(b)) => (a, b),
        _ => return,
    };
    let v2_bad_stack = with_zero_max_stack(&v2, "foo");

    let mut cm = ClassManager::new(&[], &[], &[]);
    let cid = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");

    let err = cm
        .redefine_class(cid, v2_bad_stack, RedefineOptions::default())
        .expect_err("an under-declared max_stack must still be rejected here");
    let s = format!("{err:?}");
    assert!(
        s.contains("failed bytecode verification"),
        "expected a verification rejection, got: {s}"
    );
    assert_eq!(
        cm.class_redefine_generation(cid),
        0,
        "a rejected redefine must not bump the generation",
    );
}

#[test]
fn redefine_still_rejects_structurally_broken_bytes_under_a_user_loader() {
    // The deferral is about the *type-state* verdict only. The
    // hierarchy-independent structural scan is what stands between a JVMTI agent
    // and the interpreter dispatching into the middle of an instruction or
    // reading a local slot the frame does not have, and it must still run.
    let (v1, v2) = match (load_fixture("Foo.v1.class"), load_fixture("Foo.v2.class")) {
        (Some(a), Some(b)) => (a, b),
        _ => return,
    };
    let v2_bad_locals = with_zero_max_locals(&v2, "foo");

    let mut cm = ClassManager::new(&[], &[], &[]);
    let cid = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::UserDefined(9),
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");

    let err = cm
        .redefine_class(cid, v2_bad_locals, RedefineOptions::default())
        .expect_err("structurally broken bytes must be rejected under any loader");
    let s = format!("{err:?}");
    assert!(
        s.contains("failed bytecode verification"),
        "expected a verification rejection, got: {s}"
    );
}
