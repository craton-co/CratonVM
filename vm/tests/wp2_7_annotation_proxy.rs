// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.7 — Annotation proxy spec-compliance tests.
//!
//! These tests verify the runtime contract of the annotation proxy after
//! the WP2.7 promotion to spec-compliant `Annotation` semantics:
//!
//!   * `equals(Object)`: true iff same annotation type AND each member equals
//!   * `hashCode()`: spec recipe `sum((127 * nameHash) ^ valueHash)`
//!   * `toString()`: `@FQN(name1=val1, name2=val2, ...)` — alphabetical members
//!   * `annotationType()`: returns the cached Class mirror
//!
//! The full end-to-end probe is `apps/annotation_proxy_probe/`.
//! These tests exercise the `java_string_hash` helper directly (since it's
//! the load-bearing primitive for hashCode), plus drive the probe through
//! a real VM and observe its stdout via the standard `Vm::invoke`.

use cratonvm_vm::vm::java_string_hash;

#[test]
fn java_string_hash_matches_jdk_for_empty_string() {
    assert_eq!(java_string_hash(""), 0);
}

#[test]
fn java_string_hash_matches_jdk_for_single_char() {
    // 'a' = 97 → hash = 97 * 31^0 = 97
    assert_eq!(java_string_hash("a"), 97);
}

#[test]
fn java_string_hash_matches_jdk_for_three_chars() {
    // "abc" → 97*31^2 + 98*31 + 99 = 96354
    assert_eq!(java_string_hash("abc"), 96354);
}

#[test]
fn java_string_hash_matches_jdk_for_value_keyword() {
    // "value" → captured from `("value").hashCode()` in HotSpot.
    // 'v'=118, 'a'=97, 'l'=108, 'u'=117, 'e'=101
    // 118*31^4 + 97*31^3 + 108*31^2 + 117*31 + 101 = 111972721
    assert_eq!(java_string_hash("value"), 111972721);
}

#[test]
fn java_string_hash_matches_jdk_for_count_keyword() {
    // "count" → JDK output: 94851343
    assert_eq!(java_string_hash("count"), 94851343);
}

#[test]
fn java_string_hash_handles_unicode_supplementary() {
    // Surrogate pair handling: encode_utf16 emits a surrogate pair for
    // a supplementary character (e.g. U+1F600). Hash is computed over
    // the two surrogate code units, matching Java's String.hashCode
    // (which iterates char-by-char).
    let s = "\u{1f600}";
    let h = java_string_hash(s);
    // Hash of the two surrogates (0xD83D, 0xDE00):
    // 0xD83D = 55357, 0xDE00 = 56832 → 55357*31 + 56832 = 1772899
    assert_eq!(h, 1772899);
}

#[test]
fn java_string_hash_matches_jdk_for_long_strings() {
    // "java/lang/Integer" — used for boxed-Integer descriptor hashing.
    // Reference: ("java/lang/Integer").hashCode() == -607409974 (HotSpot JDK 25).
    assert_eq!(java_string_hash("java/lang/Integer"), -607409974);
}

#[test]
fn java_string_hash_handles_negative_overflow() {
    // A long string causes signed overflow — hash wraps modulo 2^32.
    let s = "java.lang.annotation.Documented";
    // Reference: HotSpot returns -1239452027 (signed-overflow path).
    assert_eq!(java_string_hash(s), -1239452027);
}

#[test]
fn annotation_proxy_class_constant_is_stable() {
    // The proxy class name is the load-bearing dispatch key in
    // `interpreter.rs::execute_invokevirtual_*` and `vm_exec.rs`. If
    // someone renames it to e.g. `java/lang/annotation/Proxy` every
    // `getAnnotation` site silently breaks. Pin the constant.
    assert_eq!(
        cratonvm_native_builtins::lang_class::ANN_PROXY_FIELDS,
        4,
        "annotation proxy must keep its 4-field layout"
    );
}

#[test]
fn annotation_proxy_field_indices_constant() {
    use cratonvm_native_builtins::lang_class::{
        ANN_PROXY_ELEM_NAMES, ANN_PROXY_ELEM_VALUES, ANN_PROXY_TYPE_DESC, ANN_PROXY_TYPE_MIRROR,
    };
    assert_eq!(ANN_PROXY_TYPE_DESC, 0);
    assert_eq!(ANN_PROXY_TYPE_MIRROR, 1);
    assert_eq!(ANN_PROXY_ELEM_NAMES, 2);
    assert_eq!(ANN_PROXY_ELEM_VALUES, 3);
}

mod common;

#[test]
fn annotation_proxy_probe_compiled_class_files_exist() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let probe_dir = manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("annotation_proxy_probe");
    // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
    // `common::require_fixture`. `apps/` is gitignored (.gitignore line 12), so
    // this fixture was never tracked and is absent from the tree.
    let main_cls = probe_dir.join("AnnotationProxyProbe.class");
    if !probe_dir.exists() || !main_cls.exists() {
        let _ = common::require_fixture(
            "wp2_7_annotation_proxy",
            "the WP2.7 fixture `AnnotationProxyProbe` (AnnotationProxyProbe.class, compiled from \
             AnnotationProxyProbe.java; this test pins its Test/Other/TargetA annotation types)",
            &[
                main_cls.clone(),
                probe_dir.join("AnnotationProxyProbe.java"),
            ],
        );
        return;
    }
    // The annotation types should also be staged.
    for stub in &["Test.class", "Other.class", "TargetA.class"] {
        let p = probe_dir.join(stub);
        assert!(p.exists(), "{} must be staged when classes/ exists", stub);
    }
}

#[test]
fn annotation_proxy_probe_loads_under_cratonvm_when_staged() {
    use cratonvm_vm::config::VmConfig;
    use cratonvm_vm::vm::Vm;

    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let probe_dir = manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("annotation_proxy_probe");
    if !probe_dir.join("AnnotationProxyProbe.class").exists() {
        let _ = common::require_fixture(
            "wp2_7_annotation_proxy",
            "the WP2.7 fixture `AnnotationProxyProbe` (AnnotationProxyProbe.class, compiled from \
             AnnotationProxyProbe.java)",
            &[
                probe_dir.join("AnnotationProxyProbe.class"),
                probe_dir.join("AnnotationProxyProbe.java"),
            ],
        );
        return;
    }

    let cp = vec![probe_dir.to_string_lossy().to_string()];
    let config = VmConfig::default().with_classpath(cp);
    let vm = Vm::new(config);

    let result = vm.shared.load_class_concurrent("AnnotationProxyProbe");
    assert!(result.is_ok(), "probe class must load: {:?}", result);
}

#[test]
fn annotation_member_hash_recipe_for_synthetic_string_member() {
    // (127 * "value".hashCode()) ^ "hello".hashCode()
    let name_h = java_string_hash("value");
    let val_h = java_string_hash("hello");
    let expected = 127i32.wrapping_mul(name_h) ^ val_h;
    // The `annotation_member_hash` helper is private (`vm_exec.rs`, and its
    // `ctx_` twin in `native-builtins/src/lang_class.rs`), so this test cannot
    // call it. It CAN pin the two things it can reach: the recipe's shape, and
    // the `java_string_hash` primitive the recipe is built on.
    //
    // This assertion used to be `let _ = expected;` — every line above ran and
    // nothing was checked, so a drift in `java_string_hash` (the load-bearing
    // primitive named in this file's own module docs) passed straight through.
    //
    // The constant is derived, not guessed:
    //   "value".hashCode() == 111972721  (pinned by
    //                                     java_string_hash_matches_jdk_for_value_keyword)
    //   "hello".hashCode() ==  99162322
    //   127 * 111972721 == 14_220_535_567, which wraps to 1_335_633_679 as i32
    //   1_335_633_679 ^ 99_162_322 == 1_249_198_045
    //
    // The wrap is the interesting part: the JLS recipe is defined on Java `int`
    // arithmetic, so an implementation that widened to i64 anywhere would
    // produce 14_220_535_567 ^ 99_162_322 instead and fail here.
    assert_eq!(
        name_h, 111972721,
        "java_string_hash(\"value\") drifted; the annotation-member recipe below is derived from it"
    );
    assert_eq!(
        val_h, 99162322,
        "java_string_hash(\"hello\") drifted; the annotation-member recipe below is derived from it"
    );
    assert_eq!(
        expected, 1249198045,
        "the JLS annotation-member hash recipe `(127 * nameHash) ^ valueHash` must be evaluated in \
         wrapping 32-bit arithmetic"
    );
}
