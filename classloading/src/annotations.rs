// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
            Some(Attribute::AnnotationDefault(ev)) => Some(&**ev),
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
mod tests;
