use super::*;
use cratonvm_reader::attribute::{
    Annotation, Attribute, ElementValue, ElementValuePair, LazyAttribute, TypeAnnotation,
    TypePathEntry,
};

fn decoded(attrs: Vec<Attribute>) -> Vec<LazyAttribute> {
    attrs.into_iter().map(LazyAttribute::new_decoded).collect()
}

fn make_ann(type_index: u16, pairs: Vec<ElementValuePair>) -> Annotation {
    Annotation {
        type_index,
        element_value_pairs: pairs,
    }
}

fn make_pair(name_index: u16, value: ElementValue) -> ElementValuePair {
    ElementValuePair {
        element_name_index: name_index,
        value,
    }
}

#[test]
fn runtime_visible_is_iterable() {
    let attrs = decoded(vec![Attribute::RuntimeVisibleAnnotations(vec![
        make_ann(1, vec![]),
        make_ann(2, vec![]),
    ])]);
    let v = AnnotationsView::new(&attrs);
    let indices: Vec<u16> = v.runtime_visible().map(|a| a.type_index).collect();
    assert_eq!(indices, vec![1, 2]);
}

#[test]
fn runtime_invisible_is_separate_from_visible() {
    let attrs = decoded(vec![
        Attribute::RuntimeVisibleAnnotations(vec![make_ann(1, vec![])]),
        Attribute::RuntimeInvisibleAnnotations(vec![make_ann(2, vec![])]),
    ]);
    let v = AnnotationsView::new(&attrs);
    let visible: Vec<_> = v.runtime_visible().map(|a| a.type_index).collect();
    let invisible: Vec<_> = v.runtime_invisible().map(|a| a.type_index).collect();
    assert_eq!(visible, vec![1]);
    assert_eq!(invisible, vec![2]);
}

#[test]
fn all_chains_visible_then_invisible() {
    let attrs = decoded(vec![
        Attribute::RuntimeVisibleAnnotations(vec![make_ann(1, vec![])]),
        Attribute::RuntimeInvisibleAnnotations(vec![make_ann(2, vec![])]),
    ]);
    let v = AnnotationsView::new(&attrs);
    let all: Vec<_> = v.all().map(|a| a.type_index).collect();
    assert_eq!(all, vec![1, 2]);
}

#[test]
fn find_by_type_descriptor_matches_only_visible() {
    let attrs = decoded(vec![
        Attribute::RuntimeVisibleAnnotations(vec![make_ann(1, vec![])]),
        Attribute::RuntimeInvisibleAnnotations(vec![make_ann(2, vec![])]),
    ]);
    let v = AnnotationsView::new(&attrs);
    let pool = |idx: u16| match idx {
        1 => Some("Lfoo/Visible;"),
        2 => Some("Lfoo/Invisible;"),
        _ => None,
    };
    let found = v.find_by_type_descriptor("Lfoo/Visible;", pool);
    assert!(found.is_some());
    let missed = v.find_by_type_descriptor("Lfoo/Invisible;", pool);
    assert!(missed.is_none(), "invisible must not be matched");
}

#[test]
fn parameter_annotations_returns_row_count() {
    let rows = vec![
        vec![make_ann(1, vec![])],
        vec![],
        vec![make_ann(2, vec![]), make_ann(3, vec![])],
    ];
    let attrs = decoded(vec![Attribute::RuntimeVisibleParameterAnnotations(
        rows.clone(),
    )]);
    let v = AnnotationsView::new(&attrs);
    let got = v.parameter_annotations().unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].len(), 1);
    assert_eq!(got[1].len(), 0);
    assert_eq!(got[2].len(), 2);
}

#[test]
fn parameter_annotations_falls_back_to_invisible() {
    let attrs = decoded(vec![Attribute::RuntimeInvisibleParameterAnnotations(vec![
        vec![make_ann(5, vec![])],
    ])]);
    let v = AnnotationsView::new(&attrs);
    let got = v.parameter_annotations().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0].type_index, 5);
}

#[test]
fn parameter_annotations_missing_returns_none() {
    let attrs: Vec<LazyAttribute> = Vec::new();
    let v = AnnotationsView::new(&attrs);
    assert!(v.parameter_annotations().is_none());
}

#[test]
fn type_annotations_are_enumerated() {
    let ta = TypeAnnotation {
        target_type: 0x13,
        target_info: vec![],
        type_path: vec![TypePathEntry {
            type_path_kind: 3,
            type_argument_index: 0,
        }],
        annotation: make_ann(1, vec![]),
    };
    let attrs = decoded(vec![Attribute::RuntimeVisibleTypeAnnotations(vec![ta])]);
    let v = AnnotationsView::new(&attrs);
    assert_eq!(v.type_annotations().count(), 1);
}

#[test]
fn annotation_default_is_returned_if_present() {
    let ev = ElementValue::Const {
        tag: b's',
        const_value_index: 7,
    };
    let attrs = decoded(vec![Attribute::AnnotationDefault(Box::new(ev.clone()))]);
    let v = AnnotationsView::new(&attrs);
    match v.annotation_default() {
        Some(ElementValue::Const {
            tag,
            const_value_index,
        }) => {
            assert_eq!(*tag, b's');
            assert_eq!(*const_value_index, 7);
        }
        other => panic!("expected Const, got {other:?}"),
    }
}

#[test]
fn annotation_default_is_none_when_absent() {
    let attrs: Vec<LazyAttribute> = Vec::new();
    let v = AnnotationsView::new(&attrs);
    assert!(v.annotation_default().is_none());
}

#[test]
fn descriptor_round_trip() {
    let desc = class_name_to_annotation_descriptor("java/lang/Override");
    assert_eq!(desc, "Ljava/lang/Override;");
    let back = annotation_descriptor_to_class_name(&desc).unwrap();
    assert_eq!(back, "java/lang/Override");
}

#[test]
fn descriptor_reverse_rejects_malformed() {
    assert!(annotation_descriptor_to_class_name("java/lang/Override").is_none());
    assert!(annotation_descriptor_to_class_name("Ljava/lang/Override").is_none());
    assert!(annotation_descriptor_to_class_name("java/lang/Override;").is_none());
}

#[test]
fn annotation_type_matches_resolves_via_pool_lookup() {
    let ann = make_ann(7, vec![]);
    let pool = |idx: u16| match idx {
        7 => Some("Lfoo/Bar;".to_string()),
        _ => None,
    };
    assert!(annotation_type_matches(&ann, "Lfoo/Bar;", pool));
    let pool2 = |idx: u16| match idx {
        7 => Some("Lfoo/Bar;".to_string()),
        _ => None,
    };
    assert!(!annotation_type_matches(&ann, "Lfoo/Other;", pool2));
}

#[test]
fn element_value_const_tags_all_representable() {
    // Every JVMS §4.7.16.1 tag is expressible via `ElementValue::Const`.
    for tag in *b"BCDFIJSZs" {
        let pair = make_pair(
            1,
            ElementValue::Const {
                tag,
                const_value_index: 2,
            },
        );
        let ann = make_ann(0, vec![pair]);
        assert_eq!(ann.element_value_pairs.len(), 1);
        match &ann.element_value_pairs[0].value {
            ElementValue::Const { tag: got, .. } => assert_eq!(*got, tag),
            _ => panic!("expected Const"),
        }
    }
}

#[test]
fn element_value_array_nests_annotations() {
    // An @Outer({@Inner("a"), @Inner("b")}) style payload — nested
    // annotations inside an array element.
    let inner_a = make_ann(
        3,
        vec![make_pair(
            4,
            ElementValue::Const {
                tag: b's',
                const_value_index: 5,
            },
        )],
    );
    let inner_b = make_ann(
        3,
        vec![make_pair(
            4,
            ElementValue::Const {
                tag: b's',
                const_value_index: 6,
            },
        )],
    );
    let arr = ElementValue::Array(vec![
        ElementValue::AnnotationValue(inner_a),
        ElementValue::AnnotationValue(inner_b),
    ]);
    let outer = make_ann(1, vec![make_pair(2, arr)]);
    match &outer.element_value_pairs[0].value {
        ElementValue::Array(items) => {
            assert_eq!(items.len(), 2);
            for item in items {
                match item {
                    ElementValue::AnnotationValue(inner) => {
                        assert_eq!(inner.type_index, 3);
                    }
                    _ => panic!("expected nested annotation"),
                }
            }
        }
        _ => panic!("expected Array"),
    }
}
