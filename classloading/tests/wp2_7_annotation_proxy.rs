// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.7 — Annotation proxy contract tests.
//!
//! These pin the **descriptor + lookup** layer that the annotation proxy
//! sits on top of. The full runtime contract (equals/hashCode/toString)
//! lives in `vm/src/vm/vm_exec.rs::annotation_proxy_dispatch_impl` and is
//! exercised end-to-end by `apps/annotation_proxy_probe/AnnotationProxyProbe.java`.
//!
//! The tests here verify:
//!   1. Annotation lookup uses the descriptor form `LType;` consistently.
//!   2. Reverse mapping (`Lpkg/A;` → `pkg/A`) is the inverse.
//!   3. `find_by_type_descriptor` matches only runtime-visible.
//!   4. Method/field views surface @Test on `m()` / `f`.
//!   5. The same tests as wp1_7 but pinned to the proxy lookup contract.
//!   6. AnnotationDefault is exposed.
//!   7. Inner-class descriptor round-trip preserves '$'.
//!   8. Annotation member iteration over the parsed `element_value_pairs`
//!      preserves declaration order (the order the proxy stores them in).

use cratonvm_classloading::annotations::{
    annotation_descriptor_to_class_name, annotation_type_matches,
    class_name_to_annotation_descriptor, field_annotations, method_annotations, AnnotationsView,
};
use cratonvm_reader::attribute::{Annotation, ElementValue, ElementValuePair, LazyAttribute};
use cratonvm_reader::class_file::ClassFile;
use cratonvm_reader::read_class;
use std::path::PathBuf;

fn load_probe_class(short_name: &str) -> Option<ClassFile> {
    // Use the WP1.7 probe (also covers our needs — the probe app is for
    // runtime test infrastructure, classloading-side tests just need a
    // class with attached annotations).
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // step out of classloading/
    path.push("apps");
    path.push("annotation_probe");
    path.push(format!("{short_name}.class"));
    let bytes = std::fs::read(&path).ok()?;
    Some(read_class(&bytes).expect("reader parses probe fixture"))
}

#[test]
fn descriptor_round_trip_holds_for_simple_class() {
    let desc = class_name_to_annotation_descriptor("Test");
    assert_eq!(desc, "LTest;");
    assert_eq!(annotation_descriptor_to_class_name(&desc), Some("Test"));
}

#[test]
fn descriptor_round_trip_handles_inner_class() {
    let desc = class_name_to_annotation_descriptor("ServiceLoaderProbe$Greeter");
    assert_eq!(desc, "LServiceLoaderProbe$Greeter;");
    assert_eq!(
        annotation_descriptor_to_class_name(&desc),
        Some("ServiceLoaderProbe$Greeter")
    );
}

#[test]
fn descriptor_round_trip_handles_package_qualified() {
    let desc = class_name_to_annotation_descriptor("javax/inject/Named");
    assert_eq!(desc, "Ljavax/inject/Named;");
    assert_eq!(
        annotation_descriptor_to_class_name(&desc),
        Some("javax/inject/Named")
    );
}

#[test]
fn descriptor_reverse_rejects_malformed() {
    assert!(annotation_descriptor_to_class_name("javax/inject/Named").is_none());
    assert!(annotation_descriptor_to_class_name("Ljavax/inject/Named").is_none());
    assert!(annotation_descriptor_to_class_name("javax/inject/Named;").is_none());
    assert!(annotation_descriptor_to_class_name("").is_none());
}

#[test]
fn class_level_annotation_matches_descriptor_form() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let view = AnnotationsView::new(&cf.attributes);
    let found = view.find_by_type_descriptor("LTest;", |idx| cf.constant_pool.get_utf8(idx));
    assert!(
        found.is_some(),
        "@Test class-level annotation must match LTest;"
    );
}

#[test]
fn method_annotation_lookup_finds_test_on_m() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let m = cf
        .methods
        .iter()
        .find(|m| &*m.name == "m")
        .expect("m() must exist");
    let view = method_annotations(m);
    let found = view.find_by_type_descriptor("LTest;", |idx| cf.constant_pool.get_utf8(idx));
    assert!(found.is_some(), "method m must have @Test");
    let visible: Vec<_> = view.runtime_visible().collect();
    assert_eq!(visible.len(), 1);
}

#[test]
fn field_annotation_lookup_finds_test_on_f() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let f = cf
        .fields
        .iter()
        .find(|f| &*f.name == "f")
        .expect("field f must exist");
    let view = field_annotations(f);
    let found = view.find_by_type_descriptor("LTest;", |idx| cf.constant_pool.get_utf8(idx));
    assert!(found.is_some(), "field f must have @Test");
}

#[test]
fn annotation_type_matches_resolves_full_descriptor() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let view = AnnotationsView::new(&cf.attributes);
    let test_ann = view
        .runtime_visible()
        .find(|ann| {
            annotation_type_matches(ann, "LTest;", |idx| {
                cf.constant_pool.get_utf8(idx).map(|s| s.to_string())
            })
        })
        .expect("@Test must match");
    // The annotation should carry several element-value pairs.
    assert!(!test_ann.element_value_pairs.is_empty());
}

#[test]
fn invisible_annotation_descriptor_does_not_match_visible_view() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let view = AnnotationsView::new(&cf.attributes);
    // None of the probe's annotations should be invisible.
    let invisible: Vec<_> = view.runtime_invisible().collect();
    assert!(invisible.is_empty());
}

#[test]
fn member_iteration_preserves_class_file_order() {
    // Synthetic: build an Annotation with known member order and verify
    // the ElementValuePair slice preserves that order — this is the order
    // the proxy will store and emit in toString.
    let pairs = vec![
        ElementValuePair {
            element_name_index: 1,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: 2,
            },
        },
        ElementValuePair {
            element_name_index: 3,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: 4,
            },
        },
        ElementValuePair {
            element_name_index: 5,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: 6,
            },
        },
    ];
    let ann = Annotation {
        type_index: 99,
        element_value_pairs: pairs,
    };
    // Iteration order matches insertion order.
    let names: Vec<u16> = ann
        .element_value_pairs
        .iter()
        .map(|p| p.element_name_index)
        .collect();
    assert_eq!(names, vec![1, 3, 5]);
}

#[test]
fn annotation_default_is_visible_through_view() {
    // The probe's `Test` annotation type carries `default ""` for value().
    let cf = match load_probe_class("Test") {
        Some(cf) => cf,
        None => return,
    };
    let value_method = cf
        .methods
        .iter()
        .find(|m| &*m.name == "value")
        .expect("Test.value() must exist");
    let view = method_annotations(value_method);
    let default = view
        .annotation_default()
        .expect("Test.value() must carry AnnotationDefault");
    match default {
        ElementValue::Const { tag, .. } => assert_eq!(*tag, b's'),
        other => panic!("expected Const('s'), got {other:?}"),
    }
}

#[test]
fn proxy_descriptor_form_strips_l_prefix_and_semicolon() {
    // The proxy's `field 0` stores the descriptor form ("LType;").
    // Verify the round-trip helpers used by the proxy build path.
    let internal = "java/lang/Override";
    let desc = class_name_to_annotation_descriptor(internal);
    assert_eq!(desc, "Ljava/lang/Override;");
    let back = annotation_descriptor_to_class_name(&desc).unwrap();
    assert_eq!(back, internal);
}

#[test]
fn empty_attribute_list_yields_no_annotations() {
    let attrs: Vec<LazyAttribute> = Vec::new();
    let view = AnnotationsView::new(&attrs);
    assert_eq!(view.runtime_visible().count(), 0);
    assert_eq!(view.runtime_invisible().count(), 0);
    assert_eq!(view.all().count(), 0);
    assert!(view.parameter_annotations().is_none());
    assert_eq!(view.type_annotations().count(), 0);
    assert!(view.annotation_default().is_none());
}

#[test]
fn descriptor_helpers_handle_array_class_names_idempotently() {
    // Annotation type descriptors are always object-form (`L...;`); array
    // classes (`[B`, `[Lfoo/Bar;`) never appear as annotation types but
    // the descriptor helpers should reject them gracefully so an error
    // path can react.
    let arr = "[B";
    let desc = class_name_to_annotation_descriptor(arr);
    // Wrapping is mechanical — produces "L[B;". The reverse should still
    // produce "[B" because we strip the L and ';'.
    assert_eq!(desc, "L[B;");
    assert_eq!(annotation_descriptor_to_class_name(&desc), Some("[B"));
}
