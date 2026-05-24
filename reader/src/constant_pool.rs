// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::Arc;

/// Represents an entry in the constant pool of a `.class` file.
///
/// The constant pool is a table of structures representing string constants,
/// class and interface names, field names, and other constants referred to
/// within the class file (JVM spec 4.4).
#[derive(Debug, Clone)]
pub enum ConstantPoolEntry {
    /// Placeholder for the unused index 0 and the second slot of Long/Double entries.
    Tombstone,

    /// CONSTANT_Utf8 (tag 1): A UTF-8 encoded string.
    ///
    /// Stored as `Arc<str>` so identical UTF-8 content shares a single backing
    /// allocation across every constant pool that references it. See
    /// [`cratonvm_types::intern_arc`] — the class-reader hot path funnels every
    /// UTF-8 entry through the global string pool so the same class/method
    /// name loaded via many class files occupies memory exactly once.
    Utf8(Arc<str>),

    /// CONSTANT_Integer (tag 3): A 4-byte integer constant.
    Integer(i32),

    /// CONSTANT_Float (tag 4): A 4-byte float constant.
    Float(f32),

    /// CONSTANT_Long (tag 5): An 8-byte long constant. Takes two constant pool slots.
    Long(i64),

    /// CONSTANT_Double (tag 6): An 8-byte double constant. Takes two constant pool slots.
    Double(f64),

    /// CONSTANT_Class (tag 7): A symbolic reference to a class or interface.
    ClassReference { name_index: u16 },

    /// CONSTANT_String (tag 8): A constant string value.
    StringReference { string_index: u16 },

    /// CONSTANT_Fieldref (tag 9): A symbolic reference to a field.
    FieldReference {
        class_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_Methodref (tag 10): A symbolic reference to a class method.
    MethodReference {
        class_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_InterfaceMethodref (tag 11): A symbolic reference to an interface method.
    InterfaceMethodReference {
        class_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_NameAndType (tag 12): A field or method name and descriptor.
    NameAndType {
        name_index: u16,
        descriptor_index: u16,
    },

    /// CONSTANT_MethodHandle (tag 15): A method handle (Java 7+).
    MethodHandle {
        reference_kind: u8,
        reference_index: u16,
    },

    /// CONSTANT_MethodType (tag 16): A method type (Java 7+).
    MethodType { descriptor_index: u16 },

    /// CONSTANT_Dynamic (tag 17): A dynamically-computed constant (Java 11+).
    Dynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_InvokeDynamic (tag 18): A dynamically-computed call site (Java 7+).
    InvokeDynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_Module (tag 19): A module (Java 9+).
    Module { name_index: u16 },

    /// CONSTANT_Package (tag 20): A package (Java 9+).
    Package { name_index: u16 },
}

/// The constant pool of a class file.
///
/// Uses 1-based indexing as per the JVM specification. Index 0 is always a Tombstone.
/// Long and Double entries occupy two slots (the second is also a Tombstone).
#[derive(Debug)]
pub struct ConstantPool {
    entries: Vec<ConstantPoolEntry>,
}

impl ConstantPool {
    pub fn new(entries: Vec<ConstantPoolEntry>) -> Self {
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() <= 1
    }

    /// Get an entry by 1-based index.
    pub fn get(&self, index: u16) -> Option<&ConstantPoolEntry> {
        self.entries.get(index as usize)
    }

    /// Get a UTF-8 string by 1-based index.
    pub fn get_utf8(&self, index: u16) -> Option<&str> {
        match self.get(index) {
            Some(ConstantPoolEntry::Utf8(s)) => Some(s.as_ref()),
            _ => None,
        }
    }

    /// Get a UTF-8 string by 1-based index as a shared `Arc<str>`.
    ///
    /// Returns a clone of the interned backing `Arc<str>` — callers that need
    /// to hold onto the string beyond the lifetime of the constant pool
    /// should use this instead of [`get_utf8`]. No re-allocation occurs; the
    /// clone is a single refcount bump. Originates from
    /// [`cratonvm_types::intern_arc`] at parse time.
    pub fn get_utf8_arc(&self, index: u16) -> Option<Arc<str>> {
        match self.get(index) {
            Some(ConstantPoolEntry::Utf8(s)) => Some(Arc::clone(s)),
            _ => None,
        }
    }

    /// Get a class name by resolving a ClassReference at the given index.
    pub fn get_class_name(&self, index: u16) -> Option<&str> {
        match self.get(index) {
            Some(ConstantPoolEntry::ClassReference { name_index }) => self.get_utf8(*name_index),
            _ => None,
        }
    }

    /// Get a class name by resolving a ClassReference, as a shared `Arc<str>`.
    ///
    /// Same as [`get_class_name`] but returns the interned `Arc<str>` directly
    /// (a refcount bump rather than an allocation). Hot path for the bytecode
    /// verifier, which materialises `VType::ObjectRef(Arc<str>)` for every
    /// `new` / `checkcast` / exception-handler catch type.
    pub fn get_class_name_arc(&self, index: u16) -> Option<Arc<str>> {
        match self.get(index) {
            Some(ConstantPoolEntry::ClassReference { name_index }) => {
                self.get_utf8_arc(*name_index)
            }
            _ => None,
        }
    }

    /// Get the name and descriptor from a NameAndType entry.
    pub fn get_name_and_type(&self, index: u16) -> Option<(&str, &str)> {
        match self.get(index) {
            Some(ConstantPoolEntry::NameAndType {
                name_index,
                descriptor_index,
            }) => {
                let name = self.get_utf8(*name_index)?;
                let descriptor = self.get_utf8(*descriptor_index)?;
                Some((name, descriptor))
            }
            _ => None,
        }
    }

    /// Validate that all cross-references in the constant pool point to entries
    /// of the correct type. Returns a list of validation errors.
    ///
    /// This catches malformed class files where, e.g., a ClassReference's
    /// `name_index` doesn't point to a Utf8 entry.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let len = self.entries.len() as u16;

        for (i, entry) in self.entries.iter().enumerate() {
            match entry {
                ConstantPoolEntry::ClassReference { name_index } => {
                    if *name_index == 0 || *name_index >= len {
                        errors.push(format!(
                            "cp#{i}: ClassReference name_index {} out of bounds",
                            name_index
                        ));
                    } else if !matches!(self.entries.get(*name_index as usize), Some(ConstantPoolEntry::Utf8(_))) {
                        errors.push(format!(
                            "cp#{i}: ClassReference name_index {} does not point to Utf8",
                            name_index
                        ));
                    }
                }
                ConstantPoolEntry::StringReference { string_index } => {
                    if *string_index == 0 || *string_index >= len {
                        errors.push(format!(
                            "cp#{i}: StringReference string_index {} out of bounds",
                            string_index
                        ));
                    } else if !matches!(self.entries.get(*string_index as usize), Some(ConstantPoolEntry::Utf8(_))) {
                        errors.push(format!(
                            "cp#{i}: StringReference string_index {} does not point to Utf8",
                            string_index
                        ));
                    }
                }
                ConstantPoolEntry::FieldReference { class_index, name_and_type_index }
                | ConstantPoolEntry::MethodReference { class_index, name_and_type_index }
                | ConstantPoolEntry::InterfaceMethodReference { class_index, name_and_type_index } => {
                    if *class_index == 0 || *class_index >= len {
                        errors.push(format!("cp#{i}: class_index {} out of bounds", class_index));
                    } else if !matches!(self.entries.get(*class_index as usize), Some(ConstantPoolEntry::ClassReference { .. })) {
                        errors.push(format!(
                            "cp#{i}: class_index {} does not point to ClassReference",
                            class_index
                        ));
                    }
                    if *name_and_type_index == 0 || *name_and_type_index >= len {
                        errors.push(format!(
                            "cp#{i}: name_and_type_index {} out of bounds",
                            name_and_type_index
                        ));
                    } else if !matches!(self.entries.get(*name_and_type_index as usize), Some(ConstantPoolEntry::NameAndType { .. })) {
                        errors.push(format!(
                            "cp#{i}: name_and_type_index {} does not point to NameAndType",
                            name_and_type_index
                        ));
                    }
                }
                ConstantPoolEntry::NameAndType { name_index, descriptor_index } => {
                    if *name_index == 0 || *name_index >= len {
                        errors.push(format!("cp#{i}: NameAndType name_index {} out of bounds", name_index));
                    } else if !matches!(self.entries.get(*name_index as usize), Some(ConstantPoolEntry::Utf8(_))) {
                        errors.push(format!(
                            "cp#{i}: NameAndType name_index {} does not point to Utf8",
                            name_index
                        ));
                    }
                    if *descriptor_index == 0 || *descriptor_index >= len {
                        errors.push(format!(
                            "cp#{i}: NameAndType descriptor_index {} out of bounds",
                            descriptor_index
                        ));
                    } else if !matches!(self.entries.get(*descriptor_index as usize), Some(ConstantPoolEntry::Utf8(_))) {
                        errors.push(format!(
                            "cp#{i}: NameAndType descriptor_index {} does not point to Utf8",
                            descriptor_index
                        ));
                    }
                }
                ConstantPoolEntry::MethodType { descriptor_index } => {
                    if *descriptor_index == 0 || *descriptor_index >= len {
                        errors.push(format!(
                            "cp#{i}: MethodType descriptor_index {} out of bounds",
                            descriptor_index
                        ));
                    } else if !matches!(self.entries.get(*descriptor_index as usize), Some(ConstantPoolEntry::Utf8(_))) {
                        errors.push(format!(
                            "cp#{i}: MethodType descriptor_index {} does not point to Utf8",
                            descriptor_index
                        ));
                    }
                }
                _ => {} // Tombstone, Utf8, Integer, Float, Long, Double, MethodHandle, Dynamic, InvokeDynamic, Module, Package
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_pool_utf8_lookup() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Hello".into()),
        ];
        let pool = ConstantPool::new(entries);
        assert_eq!(pool.get_utf8(1), Some("Hello"));
        assert_eq!(pool.get_utf8(0), None);
        assert_eq!(pool.get_utf8(99), None);
    }

    #[test]
    fn constant_pool_class_name_resolution() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("java/lang/Object".into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
        ];
        let pool = ConstantPool::new(entries);
        assert_eq!(pool.get_class_name(2), Some("java/lang/Object"));
    }

    // -- Constant pool validation tests --

    #[test]
    fn validate_valid_pool() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("java/lang/Object".into()),
            ConstantPoolEntry::Utf8("test".into()),
            ConstantPoolEntry::Utf8("()V".into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::NameAndType {
                name_index: 2,
                descriptor_index: 3,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 4,
                name_and_type_index: 5,
            },
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
    }

    #[test]
    fn validate_class_ref_points_to_non_utf8() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Integer(42),
            ConstantPoolEntry::ClassReference { name_index: 1 }, // points to Integer, not Utf8
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("does not point to Utf8"));
    }

    #[test]
    fn validate_class_ref_out_of_bounds() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::ClassReference { name_index: 99 },
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("out of bounds"));
    }

    #[test]
    fn validate_method_ref_class_not_class_ref() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("test".into()),
            ConstantPoolEntry::Utf8("desc".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 2,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 1,         // points to Utf8, not ClassReference
                name_and_type_index: 3,
            },
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.contains("does not point to ClassReference")));
    }

    #[test]
    fn validate_string_ref_points_to_non_utf8() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Integer(42),
            ConstantPoolEntry::StringReference { string_index: 1 },
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("does not point to Utf8"));
    }

    #[test]
    fn validate_name_and_type_wrong_types() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Integer(1),
            ConstantPoolEntry::Float(1.0),
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 2,
            },
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn validate_zero_index_rejected() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::ClassReference { name_index: 0 }, // index 0 is Tombstone
        ];
        let pool = ConstantPool::new(entries);
        let errors = pool.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("out of bounds"));
    }

    #[test]
    fn get_name_and_type_works() {
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("hello".into()),
            ConstantPoolEntry::Utf8("()V".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 2,
            },
        ];
        let pool = ConstantPool::new(entries);
        assert_eq!(pool.get_name_and_type(3), Some(("hello", "()V")));
        assert_eq!(pool.get_name_and_type(1), None); // Utf8, not NameAndType
    }

    #[test]
    fn pool_len_and_empty() {
        let pool = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
        assert_eq!(pool.len(), 1);
        assert!(pool.is_empty());

        let pool2 = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("x".into()),
        ]);
        assert_eq!(pool2.len(), 2);
        assert!(!pool2.is_empty());
    }
}
