// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 20 wave 11, lane pAtom (W9L2-4): the twelve `Math.*Exact` rows
//! (`{add,subtract,multiply,increment,decrement,negate}Exact`, `int` and
//! `long`) are claimed LEAF (`NativeMethodRegistry::set_leaf`) unless
//! `CRATONVM_MATH_EXACT_LEAF=0`. This pins the claim for both `Math` and
//! `StrictMath`, keeps `floorDiv` / `toIntExact` off the list, and fails if a
//! body grows a call that would break the leaf contract (the overflow arm must
//! stay an `Err(MethodCallFailed)` built from a Rust message, never a Java
//! allocation or a JNI-pending exception).

use cratonvm_native_api::NativeMethodRegistry;

const ROWS: [(&str, &str); 12] = [
    ("addExact", "(II)I"),
    ("addExact", "(JJ)J"),
    ("subtractExact", "(II)I"),
    ("subtractExact", "(JJ)J"),
    ("multiplyExact", "(II)I"),
    ("multiplyExact", "(JJ)J"),
    ("incrementExact", "(I)I"),
    ("incrementExact", "(J)J"),
    ("decrementExact", "(I)I"),
    ("decrementExact", "(J)J"),
    ("negateExact", "(I)I"),
    ("negateExact", "(J)J"),
];

fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("end of fn");
    &rest[..end]
}

#[test]
fn math_exact_rows_are_leaf_and_stay_leaf_safe() {
    if std::env::var_os("CRATONVM_MATH_EXACT_LEAF").is_some() {
        // The switch arm under test is the default; an explicit setting in the
        // test environment (either way) is an A/B run, not this pin.
        return;
    }
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let leaves = r.leaf_registrations();
    for class in ["java/lang/Math", "java/lang/StrictMath"] {
        for (m, d) in ROWS {
            let registered = r.find_with_kind(class, m, d).is_some();
            if !registered {
                continue;
            }
            assert!(
                leaves
                    .iter()
                    .any(|(c, mm, dd)| *c == class && *mm == m && *dd == d),
                "{class}.{m}{d} lost its leaf claim"
            );
        }
        for (m, d) in [("floorDiv", "(II)I"), ("toIntExact", "(J)I")] {
            assert!(
                !leaves
                    .iter()
                    .any(|(c, mm, dd)| *c == class && *mm == m && *dd == d),
                "{class}.{m}{d} became a leaf without an audit"
            );
        }
    }
    assert!(
        leaves
            .iter()
            .any(|(c, m, d)| *c == "java/lang/Math" && *m == "addExact" && *d == "(II)I"),
        "java/lang/Math.addExact(II)I is not a leaf"
    );

    let src = include_str!("../src/lang_math.rs");
    let forbidden = [
        "ctx.",
        "new_object",
        "alloc_object",
        "new_array",
        "new_ref_array",
        "create_string",
        "intern_string",
        "begin_blocking_region",
        "set_pending_exception",
        "invoke(",
        "invoke_virtual",
        "invoke_static",
        "pin_native_root",
        "ensure_class_initialized",
        "initialize_class",
        "load_class",
        "maybe_gc",
    ];
    let mut heads: Vec<String> = Vec::new();
    for op in [
        "add",
        "subtract",
        "multiply",
        "increment",
        "decrement",
        "negate",
    ] {
        for width in ["int", "long"] {
            heads.push(format!("pub(crate) fn native_math_{op}_exact_{width}("));
        }
    }
    heads.push("pub(crate) fn math_overflow_err(".to_string());
    heads.push("pub(crate) fn math_overflow_err_long(".to_string());
    for head in &heads {
        for line in body(src, head).lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            for f in forbidden {
                assert!(!t.contains(f), "`{f}` in {head}: {t}");
            }
        }
    }
}
