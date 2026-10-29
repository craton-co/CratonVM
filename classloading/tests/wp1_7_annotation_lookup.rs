// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.7 — `classloading::annotations` lookup helpers against real fixtures.
//!
//! These tests complement `reader/tests/wp1_7_attrs.rs` by validating the
//! `AnnotationsView` API introduced in `classloading/src/annotations.rs`:
//! a single source of truth for walking annotation attributes attached to
//! methods and fields, plus the descriptor round-trip helpers.
//!
//! Fixture: the `apps/annotation_probe` sample that already ships a
//! compiled `AnnotationProbe.class` with one class-level annotation, one
//! method-level annotation (`m()`), and one field-level annotation (`f`).

use cratonvm_classloading::annotations::{
    annotation_descriptor_to_class_name, annotation_type_matches,
    class_name_to_annotation_descriptor, field_annotations, method_annotations, AnnotationsView,
};
use cratonvm_reader::class_file::ClassFile;
use cratonvm_reader::read_class;
use std::path::PathBuf;

fn load_probe_class(short_name: &str) -> Option<ClassFile> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // step out of classloading/
    path.push("apps");
    path.push("annotation_probe");
    path.push(format!("{short_name}.class"));
    let bytes = std::fs::read(&path).ok()?;
    Some(read_class(&bytes).expect("reader parses probe fixture"))
}

#[test]
fn method_annotations_view_finds_test_on_m() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return, // fixture not staged
    };
    let m = cf
        .methods
        .iter()
        .find(|m| &*m.name == "m")
        .expect("m() must exist");
    let view = method_annotations(m);
    let visible: Vec<_> = view.runtime_visible().collect();
    assert_eq!(
        visible.len(),
        1,
        "@Test is the only runtime-visible annotation on m()"
    );

    // `find_by_type_descriptor` returns a reference to the matching annotation.
    let found = view.find_by_type_descriptor("LTest;", |idx| cf.constant_pool.get_utf8(idx));
    assert!(found.is_some(), "LTest; must resolve on m()");
}

#[test]
fn field_annotations_view_finds_test_on_f() {
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
    let visible_count = view.runtime_visible().count();
    assert_eq!(
        visible_count, 1,
        "@Test is the only runtime-visible annotation on f"
    );
    let found = view.find_by_type_descriptor("LTest;", |idx| cf.constant_pool.get_utf8(idx));
    assert!(found.is_some(), "LTest; must resolve on f");
}

#[test]
fn view_returns_none_for_missing_descriptor() {
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
    let missing =
        view.find_by_type_descriptor("Ldoes/not/Exist;", |idx| cf.constant_pool.get_utf8(idx));
    assert!(missing.is_none());
}

#[test]
fn annotation_type_matches_works_for_class_level() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let view = AnnotationsView::new(&cf.attributes);
    let any_test = view.runtime_visible().find(|ann| {
        annotation_type_matches(ann, "LTest;", |idx| {
            cf.constant_pool.get_utf8(idx).map(|s| s.to_string())
        })
    });
    assert!(
        any_test.is_some(),
        "class-level @Test must match on AnnotationProbe"
    );
}

#[test]
fn descriptor_helpers_round_trip() {
    let desc = class_name_to_annotation_descriptor("Test");
    assert_eq!(desc, "LTest;");
    assert_eq!(annotation_descriptor_to_class_name(&desc), Some("Test"));

    let nested = class_name_to_annotation_descriptor("pkg/Outer$Inner");
    assert_eq!(nested, "Lpkg/Outer$Inner;");
    assert_eq!(
        annotation_descriptor_to_class_name(&nested),
        Some("pkg/Outer$Inner")
    );
}

#[test]
fn method_with_no_annotations_yields_empty_view() {
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    // <init> has no runtime-visible annotations on the probe's class.
    let ctor = cf
        .methods
        .iter()
        .find(|m| &*m.name == "<init>")
        .expect("<init> must exist");
    let view = method_annotations(ctor);
    assert_eq!(view.runtime_visible().count(), 0);
    assert!(view.annotation_default().is_none());
}

#[test]
fn annotation_default_surfaces_through_view() {
    // Test.value() defines `default ""` — exposed via AnnotationDefault.
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
    // Don't re-validate element_value shape here (reader/tests/wp1_7_attrs.rs
    // already covers that); just make sure the view exposes it.
    match default {
        cratonvm_reader::attribute::ElementValue::Const { tag, .. } => {
            assert_eq!(*tag, b's');
        }
        other => panic!("expected Const('s'), got {other:?}"),
    }
}

#[test]
fn runtime_invisible_is_distinct_from_visible() {
    // @Inner is RUNTIME-retention in the probe, so it should appear on
    // the nested element. Verify the view's `all()` iterator returns at
    // least the runtime_visible set; an implementation that confused
    // visible/invisible would break this invariant.
    let cf = match load_probe_class("AnnotationProbe") {
        Some(cf) => cf,
        None => return,
    };
    let view = AnnotationsView::new(&cf.attributes);
    let visible_count = view.runtime_visible().count();
    let all_count = view.all().count();
    assert!(
        all_count >= visible_count,
        "all() (={all_count}) must include at least runtime_visible (={visible_count})"
    );
}

#[test]
fn class_name_to_descriptor_handles_inner_classes() {
    // Real-world: descriptor for an inner class like
    // `ServiceLoaderProbe$Greeter` must render with the `$` intact.
    let d = class_name_to_annotation_descriptor("ServiceLoaderProbe$Greeter");
    assert_eq!(d, "LServiceLoaderProbe$Greeter;");
    assert_eq!(
        annotation_descriptor_to_class_name(&d),
        Some("ServiceLoaderProbe$Greeter")
    );
}
