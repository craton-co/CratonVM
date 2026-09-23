// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.7 — Annotation attribute helpers.
//!
//! This module provides a single source of truth for walking the annotation
//! attributes that the reader parses from a `.class` file:
//!
//! - `RuntimeVisibleAnnotations` (JVMS §4.7.16) — the only ones proxies need
//!   since only `@Retention(RUNTIME)` annotation types produce this attribute
//! - `RuntimeInvisibleAnnotations` (JVMS §4.7.17) — still surfaced here so
//!   tools that want the full metadata (e.g. frameworks that treat all
//!   annotations as reflective metadata regardless of retention) can ask
//!   for "all annotations" explicitly
//! - `RuntimeVisibleParameterAnnotations` / `RuntimeInvisibleParameterAnnotations`
//!   (JVMS §4.7.18 / §4.7.19)
//! - `RuntimeVisibleTypeAnnotations` / `RuntimeInvisibleTypeAnnotations`
//!   (JVMS §4.7.20 / §4.7.21)
//! - `AnnotationDefault` (JVMS §4.7.22)
//!
//! The annotations live on:
//! - [`crate::Class::annotations`] for class-level declarations (already
//!   populated by `class_manager::define_class_with_options`)
//! - `ClassFileMethod::attributes` for method-level annotations and
//!   parameter annotations
//! - `ClassFileField::attributes` for field-level annotations
//!
//! ## Proxy lifecycle (sketch)
//!
//! A runtime annotation "proxy" is a heap object whose per-method responses
//! are backed by the pre-decoded element values from the class file. The
//! actual heap allocation + element-to-Value conversion lives in
//! `native-builtins/src/lang_class.rs::create_annotation_proxy` — this
//! module is purely the read-side metadata API that consumers (including
//! that proxy factory) call in order to pull element pairs out of the
//! parsed attribute stream.
//!
//! Until WP3.5 lands a real `java.lang.reflect.Proxy`, the proxy factory
//! synthesises a 4-field heap object whose methods are intercepted by
//! `vm_exec::annotation_proxy_invoke`. When WP3.5 lands the factory can
//! switch to building a real Proxy without touching this module.
//!
//! ## Acceptance matrix
//!
//! | Element tag | `ElementValue` variant | Producer |
//! |-------------|------------------------|----------|
//! | `B C I S Z` | `Const(tag, idx)` → `Integer` | `cp.get_integer` |
//! | `J`         | `Const('J', idx)` → `Long`  | `cp.get_long` |
//! | `F`         | `Const('F', idx)` → `Float` | `cp.get_float` |
//! | `D`         | `Const('D', idx)` → `Double`| `cp.get_double` |
//! | `s`         | `Const('s', idx)` → `String`| `cp.get_utf8` |
//! | `e`         | `Enum(type_idx, name_idx)`  | — |
//! | `c`         | `Class(idx)`                | `cp.get_utf8` |
//! | `@`         | `AnnotationValue(nested)`   | recurse |
//! | `[`         | `Array(vec)`                | recurse |
//!
//! Every tag is exercised by the integration probe at
//! `apps/annotation_probe/AnnotationProbe.java`.

use cratonvm_reader::attribute::{
    Annotation, Attribute, ElementValue, LazyAttribute, TypeAnnotation,
};
use cratonvm_reader::field::ClassFileField;
use cratonvm_reader::method::ClassFileMethod;

use crate::Class;

/// A borrowed view of the annotation data attached to a class member.
///
/// This is a zero-allocation wrapper that points back into the parsed
/// `.class`-file attribute stream. Consumers that need owned copies can
/// `.clone()` any of the returned `Annotation` values — cloning is not
/// free, but the hot reflective paths (`Class.getAnnotation(Class)`,
/// `Method.getAnnotation(Class)`) only touch the annotations whose type
/// matches a filter, so the borrow-first API minimises allocation
/// compared with the prior "build a Vec up front" style.
#[derive(Debug, Clone, Copy)]
pub struct AnnotationsView<'a> {
    attributes: &'a [LazyAttribute],
}

impl<'a> AnnotationsView<'a> {
    /// Wrap the attribute slice of a class member.
    pub fn new(attributes: &'a [LazyAttribute]) -> Self {
        Self { attributes }
    }

    /// Iterate over every `RuntimeVisibleAnnotations` entry.
    pub fn runtime_visible(self) -> impl Iterator<Item = &'a Annotation> {
        self.attributes.iter().flat_map(|a| match a.as_decoded() {
            Some(Attribute::RuntimeVisibleAnnotations(list)) => list.iter(),
            _ => (&[] as &[Annotation]).iter(),
        })
    }

    /// Iterate over every `RuntimeInvisibleAnnotations` entry.
    ///
    /// Useful for bytecode-weaving / scanning tools that treat retention
    /// purely as a hint — e.g. Weld `@Stereotype`-style introspection
    /// where the framework wants to see every annotation regardless of
    /// whether the compile-time author chose `SOURCE` / `CLASS` /
    /// `RUNTIME`.
    pub fn runtime_invisible(self) -> impl Iterator<Item = &'a Annotation> {
        self.attributes.iter().flat_map(|a| match a.as_decoded() {
            Some(Attribute::RuntimeInvisibleAnnotations(list)) => list.iter(),
            _ => (&[] as &[Annotation]).iter(),
        })
    }

    /// Iterate over both visible and invisible annotations.
    pub fn all(self) -> impl Iterator<Item = &'a Annotation> {
        self.runtime_visible().chain(self.runtime_invisible())
    }

    /// Return the first visible annotation whose type descriptor equals
    /// `"L{class_name};"` (e.g. `"Ljava/lang/Override;"`). Caller must pass
    /// the constant pool whose Utf8 slot the `type_index` points into.
    pub fn find_by_type_descriptor<F>(
        self,
        type_descriptor: &str,
        get_utf8: F,
    ) -> Option<&'a Annotation>
    where
        F: Fn(u16) -> Option<&'a str>,
    {
        self.runtime_visible()
            .find(|ann| get_utf8(ann.type_index) == Some(type_descriptor))
    }

    /// Parameter annotation rows (runtime-visible first, then invisible).
    pub fn parameter_annotations(self) -> Option<&'a [Vec<Annotation>]> {
        for a in self.attributes {
            if let Some(Attribute::RuntimeVisibleParameterAnnotations(rows)) = a.as_decoded() {
                return Some(rows.as_slice());
            }
        }
        for a in self.attributes {
            if let Some(Attribute::RuntimeInvisibleParameterAnnotations(rows)) = a.as_decoded() {
                return Some(rows.as_slice());
            }
        }
        None
    }

    /// Type-annotation rows (JVMS §4.7.20). The type-use target info is
    /// intentionally left as raw bytes — see
    /// [`cratonvm_reader::attribute::TypeAnnotation`] for the rationale.
    pub fn type_annotations(self) -> impl Iterator<Item = &'a TypeAnnotation> {
        self.attributes.iter().flat_map(|a| match a.as_decoded() {
            Some(
                Attribute::RuntimeVisibleTypeAnnotations(list)
                | Attribute::RuntimeInvisibleTypeAnnotations(list),
            ) => list.iter(),
            _ => (&[] as &[TypeAnnotation]).iter(),
        })
    }

    /// Return the `AnnotationDefault` element value (for annotation-type
    /// element methods), if present.
    pub fn annotation_default(self) -> Option<&'a ElementValue> {
        self.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::AnnotationDefault(ev)) => Some(ev),
            _ => None,
        })
    }
}

/// View the class-level annotations already extracted by the class
/// manager into `Class::annotations`.
pub fn class_runtime_annotations(class: &Class) -> &[Annotation] {
    &class.annotations
}

/// View the annotations attached to a specific method within a class.
pub fn method_annotations(method: &ClassFileMethod) -> AnnotationsView<'_> {
    AnnotationsView::new(&method.attributes)
}

/// View the annotations attached to a specific field within a class.
pub fn field_annotations(field: &ClassFileField) -> AnnotationsView<'_> {
    AnnotationsView::new(&field.attributes)
}

/// Return `true` if the annotation's type index points to a Utf8 entry
/// whose value equals `target_descriptor`. Useful when callers already
/// have the `Class` in scope and don't want to build an
/// [`AnnotationsView`].
pub fn annotation_type_matches(
    ann: &Annotation,
    target_descriptor: &str,
    get_utf8: impl Fn(u16) -> Option<String>,
) -> bool {
    matches!(get_utf8(ann.type_index), Some(s) if s == target_descriptor)
}

/// Translate an internal class name into the descriptor form that
/// annotations use as their type identifier, e.g. `"java/lang/Override"`
/// → `"Ljava/lang/Override;"`.
pub fn class_name_to_annotation_descriptor(internal_name: &str) -> String {
    let mut out = String::with_capacity(internal_name.len() + 2);
    out.push('L');
    out.push_str(internal_name);
    out.push(';');
    out
}

/// Reverse of [`class_name_to_annotation_descriptor`]. Returns `None` if
/// the descriptor is not a well-formed reference descriptor.
pub fn annotation_descriptor_to_class_name(descriptor: &str) -> Option<&str> {
    descriptor
        .strip_prefix('L')
        .and_then(|rest| rest.strip_suffix(';'))
}

#[cfg(test)]
mod tests {
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
        let attrs = decoded(vec![Attribute::AnnotationDefault(ev.clone())]);
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
}
