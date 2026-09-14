// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.7 — Annotation-attribute parsing acceptance tests.
//!
//! The reader already parses every annotation-related attribute variant
//! (see `reader/src/attribute.rs` and `reader/src/class_reader.rs`). These
//! tests pin the contract:
//!
//! * Load the real compiled `apps/annotation_probe/AnnotationProbe.class`,
//!   `Test.class`, and `Inner.class` fixtures that back the probe app.
//! * Assert that `RuntimeVisibleAnnotations` shows up on the class, on one
//!   method (`m()`), and on one field (`f`).
//! * Walk the element-value tree for every tag the probe exercises:
//!   string (`s`), int (`I`), class (`c`), enum (`e`), nested annotation
//!   (`@`), and array (`[`) — so a reader regression that silently drops
//!   any of these fails here rather than in `cargo run`.
//!
//! Fixture location is resolved relative to `CARGO_MANIFEST_DIR` so the
//! test runs from any working directory.

use cratonvm_reader::attribute::{Attribute, ElementValue, LazyAttribute};
use cratonvm_reader::class_file::ClassFile;
use cratonvm_reader::read_class;
use std::path::PathBuf;

/// Load one of the probe's compiled `.class` files.
///
/// Returns `None` when the file is missing so the test can be skipped
/// gracefully in environments where the fixture hasn't been compiled.
fn load_probe_class(short_name: &str) -> Option<ClassFile> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // step out of reader/
    path.push("apps");
    path.push("annotation_probe");
    path.push(format!("{short_name}.class"));
    let bytes = std::fs::read(&path).ok()?;
    Some(read_class(&bytes).expect("reader must parse probe fixture"))
}

/// Resolve a Utf8 entry through the constant pool (panics on bad indices
/// so the test stops at the first structural mismatch).
fn utf8<'a>(cf: &'a ClassFile, idx: u16) -> &'a str {
    cf.constant_pool
        .get_utf8(idx)
        .expect("utf8 lookup must succeed")
}

/// Extract the single `RuntimeVisibleAnnotations` attribute from a list;
/// returns an empty slice if missing.
fn visible<'a>(attrs: &'a [LazyAttribute]) -> &'a [cratonvm_reader::attribute::Annotation] {
    attrs
        .iter()
        .find_map(|a| match a.as_decoded() {
            Some(Attribute::RuntimeVisibleAnnotations(list)) => Some(list.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

#[test]
fn class_level_runtime_visible_annotation_is_parsed() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return, // fixture not staged; skip
    };

    let anns = visible(&cf.attributes);
    assert!(
        !anns.is_empty(),
        "AnnotationProbe class must carry RuntimeVisibleAnnotations"
    );
    let descriptors: Vec<String> = anns
        .iter()
        .map(|a| utf8(&cf, a.type_index).to_string())
        .collect();
    assert!(
        descriptors.iter().any(|d| d == "LTest;"),
        "expected @Test in class annotations, got {descriptors:?}"
    );
}

#[test]
fn method_level_runtime_visible_annotation_is_parsed() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let m = cf
        .methods
        .iter()
        .find(|m| &*m.name == "m")
        .expect("method m() must exist");
    let anns = visible(&m.attributes);
    assert_eq!(
        anns.len(),
        1,
        "@Test should be the only method-level annotation on m()"
    );
    assert_eq!(utf8(&cf, anns[0].type_index), "LTest;");
}

#[test]
fn field_level_runtime_visible_annotation_is_parsed() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let f = cf
        .fields
        .iter()
        .find(|f| &*f.name == "f")
        .expect("field f must exist");
    let anns = visible(&f.attributes);
    assert_eq!(
        anns.len(),
        1,
        "@Test should be the only field-level annotation on f"
    );
    assert_eq!(utf8(&cf, anns[0].type_index), "LTest;");
}

#[test]
fn class_annotation_element_values_cover_every_tag() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let anns = visible(&cf.attributes);
    let test_ann = anns
        .iter()
        .find(|a| utf8(&cf, a.type_index) == "LTest;")
        .expect("@Test must be present");

    // Collect the (name, value) pairs by element name for assertions.
    let pairs: std::collections::HashMap<String, ElementValue> = test_ann
        .element_value_pairs
        .iter()
        .map(|p| (utf8(&cf, p.element_name_index).to_string(), p.value.clone()))
        .collect();

    // string — value = "hello"
    match pairs.get("value") {
        Some(ElementValue::Const {
            tag,
            const_value_index,
        }) => {
            assert_eq!(*tag, b's', "'value' must carry string tag");
            assert_eq!(utf8(&cf, *const_value_index), "hello");
        }
        other => panic!("value must be Const('s'), got {other:?}"),
    }

    // int — count = 42
    match pairs.get("count") {
        Some(ElementValue::Const {
            tag,
            const_value_index,
        }) => {
            assert_eq!(*tag, b'I', "'count' must carry int tag");
            match cf.constant_pool.get(*const_value_index) {
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(v)) => {
                    assert_eq!(*v, 42);
                }
                other => panic!("count must resolve to Integer(42), got {other:?}"),
            }
        }
        other => panic!("count must be Const('I'), got {other:?}"),
    }

    // class — target = String.class
    match pairs.get("target") {
        Some(ElementValue::Class { class_info_index }) => {
            // `class_info_index` in an element_value actually points at a Utf8
            // descriptor per JVMS §4.7.16.1, e.g. "Ljava/lang/String;".
            let descriptor = utf8(&cf, *class_info_index);
            assert_eq!(descriptor, "Ljava/lang/String;");
        }
        other => panic!("target must be Class, got {other:?}"),
    }

    // enum — level = RetentionPolicy.RUNTIME
    match pairs.get("level") {
        Some(ElementValue::Enum {
            type_name_index,
            const_name_index,
        }) => {
            assert_eq!(
                utf8(&cf, *type_name_index),
                "Ljava/lang/annotation/RetentionPolicy;"
            );
            assert_eq!(utf8(&cf, *const_name_index), "RUNTIME");
        }
        other => panic!("level must be Enum, got {other:?}"),
    }

    // nested annotation — nested = @Inner(note = "inner")
    match pairs.get("nested") {
        Some(ElementValue::AnnotationValue(inner)) => {
            assert_eq!(utf8(&cf, inner.type_index), "LInner;");
            let note_pair = inner
                .element_value_pairs
                .iter()
                .find(|p| utf8(&cf, p.element_name_index) == "note")
                .expect("@Inner must have note element");
            match &note_pair.value {
                ElementValue::Const {
                    tag,
                    const_value_index,
                } => {
                    assert_eq!(*tag, b's');
                    assert_eq!(utf8(&cf, *const_value_index), "inner");
                }
                other => panic!("@Inner.note must be Const('s'), got {other:?}"),
            }
        }
        other => panic!("nested must be AnnotationValue, got {other:?}"),
    }

    // array — tags = {"a", "b"}
    match pairs.get("tags") {
        Some(ElementValue::Array(items)) => {
            assert_eq!(items.len(), 2, "tags array length");
            for (i, want) in ["a", "b"].iter().enumerate() {
                match &items[i] {
                    ElementValue::Const {
                        tag,
                        const_value_index,
                    } => {
                        assert_eq!(*tag, b's');
                        assert_eq!(utf8(&cf, *const_value_index), *want);
                    }
                    other => panic!("tags[{i}] must be Const('s'), got {other:?}"),
                }
            }
        }
        other => panic!("tags must be Array, got {other:?}"),
    }
}

#[test]
fn annotation_default_on_annotation_type_method() {
    // @interface Test defines `String value() default ""` — the reader
    // should expose that via AnnotationDefault on Test.value()'s method.
    let cf = match load_probe_class("Test") {
        Some(cf) => cf,
        None => return,
    };
    let method = cf
        .methods
        .iter()
        .find(|m| &*m.name == "value")
        .expect("Test.value() must exist");
    let default = method.attributes.iter().find_map(|a| match a.as_decoded() {
        Some(Attribute::AnnotationDefault(ev)) => Some(ev),
        _ => None,
    });
    let ev = default.expect("value() must carry AnnotationDefault");
    match ev {
        ElementValue::Const {
            tag,
            const_value_index,
        } => {
            assert_eq!(*tag, b's', "default of String element must be 's'");
            assert_eq!(utf8(&cf, *const_value_index), "");
        }
        other => panic!("Test.value default must be Const('s'), got {other:?}"),
    }
}

#[test]
fn retention_runtime_class_level_meta_annotation_present() {
    // `@Retention(RetentionPolicy.RUNTIME)` on the Test annotation itself —
    // this is what qualifies its instances for RuntimeVisibleAnnotations.
    let cf = match load_probe_class("Test") {
        Some(cf) => cf,
        None => return,
    };
    let anns = visible(&cf.attributes);
    let retention = anns
        .iter()
        .find(|a| utf8(&cf, a.type_index) == "Ljava/lang/annotation/Retention;")
        .expect("@Retention must be a class-level meta-annotation on @Test");

    // value = RetentionPolicy.RUNTIME
    let pair = retention
        .element_value_pairs
        .iter()
        .find(|p| utf8(&cf, p.element_name_index) == "value")
        .expect("@Retention must have value");
    match &pair.value {
        ElementValue::Enum {
            type_name_index,
            const_name_index,
        } => {
            assert_eq!(
                utf8(&cf, *type_name_index),
                "Ljava/lang/annotation/RetentionPolicy;"
            );
            assert_eq!(utf8(&cf, *const_name_index), "RUNTIME");
        }
        other => panic!("@Retention.value must be Enum, got {other:?}"),
    }
}
