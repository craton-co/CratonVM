// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVM access flags (`ACC_*`) as defined in JVMS §4.1, §4.5, §4.6, §4.7.
//!
//! These are the canonical bit patterns for class, field, and method
//! access flags. All downstream crates should consume these constants
//! rather than re-inlining the literal values. The `u16` form matches
//! the classfile encoding; the `_I32` form is convenient for the
//! reflection natives, whose on-heap `modifiers` field is `int`.
//!
//! Cross-reference: `java.lang.reflect.Modifier` exposes a matching
//! public-facing API; its constants agree bit-for-bit with these.
//!
//! Note: some flags have the same numeric value but different names
//! depending on context (e.g. `0x0040` is `ACC_VOLATILE` on a field
//! and `ACC_BRIDGE` on a method). Both names are exported from the
//! relevant sub-module.
//!
//! See also [JVMS 24 §4.1, Table 4.1-B] for class flags,
//! [JVMS 24 §4.5, Table 4.5-A] for fields, and
//! [JVMS 24 §4.6, Table 4.6-A] for methods.

// Class, field, and method — overlapping flags.
pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_PRIVATE: u16 = 0x0002;
pub const ACC_PROTECTED: u16 = 0x0004;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_FINAL: u16 = 0x0010;

// Class/method only — ACC_SYNCHRONIZED on methods, ACC_SUPER on classes.
pub const ACC_SUPER: u16 = 0x0020;
pub const ACC_SYNCHRONIZED: u16 = 0x0020;

// Field/method disambiguation at 0x0040.
pub const ACC_VOLATILE: u16 = 0x0040;
pub const ACC_BRIDGE: u16 = 0x0040;

// Field/method disambiguation at 0x0080.
pub const ACC_TRANSIENT: u16 = 0x0080;
pub const ACC_VARARGS: u16 = 0x0080;

pub const ACC_NATIVE: u16 = 0x0100;
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;
pub const ACC_STRICT: u16 = 0x0800;
pub const ACC_SYNTHETIC: u16 = 0x1000;
pub const ACC_ANNOTATION: u16 = 0x2000;
pub const ACC_ENUM: u16 = 0x4000;
pub const ACC_MODULE: u16 = 0x8000;
pub const ACC_MANDATED: u16 = 0x8000;

// Convenience: i32 copies for reflection natives whose on-heap
// `modifiers` storage is `Value::Int(i32)`.
pub const ACC_PUBLIC_I32: i32 = ACC_PUBLIC as i32;
pub const ACC_PRIVATE_I32: i32 = ACC_PRIVATE as i32;
pub const ACC_PROTECTED_I32: i32 = ACC_PROTECTED as i32;
pub const ACC_STATIC_I32: i32 = ACC_STATIC as i32;
pub const ACC_FINAL_I32: i32 = ACC_FINAL as i32;
pub const ACC_VOLATILE_I32: i32 = ACC_VOLATILE as i32;
pub const ACC_TRANSIENT_I32: i32 = ACC_TRANSIENT as i32;
pub const ACC_NATIVE_I32: i32 = ACC_NATIVE as i32;
pub const ACC_INTERFACE_I32: i32 = ACC_INTERFACE as i32;
pub const ACC_ABSTRACT_I32: i32 = ACC_ABSTRACT as i32;
pub const ACC_STRICT_I32: i32 = ACC_STRICT as i32;
pub const ACC_SYNTHETIC_I32: i32 = ACC_SYNTHETIC as i32;
pub const ACC_ANNOTATION_I32: i32 = ACC_ANNOTATION as i32;
pub const ACC_ENUM_I32: i32 = ACC_ENUM as i32;
pub const ACC_BRIDGE_I32: i32 = ACC_BRIDGE as i32;
pub const ACC_VARARGS_I32: i32 = ACC_VARARGS as i32;
pub const ACC_SYNCHRONIZED_I32: i32 = ACC_SYNCHRONIZED as i32;

/// Mask of all flags valid on a `java.lang.reflect.Modifier` output —
/// mirrors `Modifier.methodModifiers() | Modifier.classModifiers() |
/// Modifier.fieldModifiers()` in the JDK.
pub const RECOGNIZED_MODIFIERS: i32 = ACC_PUBLIC_I32
    | ACC_PRIVATE_I32
    | ACC_PROTECTED_I32
    | ACC_STATIC_I32
    | ACC_FINAL_I32
    | ACC_SYNCHRONIZED_I32
    | ACC_VOLATILE_I32
    | ACC_TRANSIENT_I32
    | ACC_NATIVE_I32
    | ACC_INTERFACE_I32
    | ACC_ABSTRACT_I32
    | ACC_STRICT_I32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_flags_agree() {
        assert_eq!(ACC_SUPER, ACC_SYNCHRONIZED);
        assert_eq!(ACC_VOLATILE, ACC_BRIDGE);
        assert_eq!(ACC_TRANSIENT, ACC_VARARGS);
        assert_eq!(ACC_MODULE, ACC_MANDATED);
    }

    #[test]
    fn i32_copies_match() {
        assert_eq!(ACC_PUBLIC_I32, ACC_PUBLIC as i32);
        assert_eq!(ACC_STATIC_I32, ACC_STATIC as i32);
        assert_eq!(ACC_FINAL_I32, ACC_FINAL as i32);
    }

    #[test]
    fn recognized_modifiers_includes_public_static_final() {
        assert_ne!(RECOGNIZED_MODIFIERS & ACC_PUBLIC_I32, 0);
        assert_ne!(RECOGNIZED_MODIFIERS & ACC_STATIC_I32, 0);
        assert_ne!(RECOGNIZED_MODIFIERS & ACC_FINAL_I32, 0);
    }
}
