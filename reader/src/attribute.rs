/// Attributes attached to class files, fields, methods, and code (JVM spec 4.7).
///
/// Attributes provide additional metadata. Some are critical for execution (Code),
/// some for debugging (LineNumberTable), and some for reflection (Signature).
#[derive(Debug, Clone)]
pub enum Attribute {
    /// The `Code` attribute (4.7.3): contains the bytecode for a method.
    Code(CodeAttribute),

    /// The `SourceFile` attribute (4.7.10): the source file name.
    SourceFile(String),

    /// The `ConstantValue` attribute (4.7.2): a constant value for a static field.
    ConstantValue { constant_value_index: u16 },

    /// The `Deprecated` attribute (4.7.15): marks a class/field/method as deprecated.
    Deprecated,

    /// The `Exceptions` attribute (4.7.5): checked exceptions a method may throw.
    Exceptions { exception_indices: Vec<u16> },

    /// The `LineNumberTable` attribute (4.7.12): maps bytecode offsets to source lines.
    LineNumberTable(Vec<LineNumberEntry>),

    /// The `InnerClasses` attribute (4.7.6): inner class information.
    InnerClasses(Vec<InnerClassInfo>),

    /// The `Signature` attribute (4.7.9): generic type signature.
    Signature(String),

    /// The `StackMapTable` attribute (4.7.4): verification type info for each basic block.
    StackMapTable { entries: Vec<u8> }, // Raw bytes for now, parsed later

    /// The `BootstrapMethods` attribute (4.7.23): bootstrap methods for invokedynamic.
    BootstrapMethods(Vec<BootstrapMethod>),

    /// The `Synthetic` attribute (4.7.8): compiler-generated member.
    Synthetic,

    /// The `EnclosingMethod` attribute (4.7.7): enclosing class/method for local/anonymous classes.
    EnclosingMethod { class_index: u16, method_index: u16 },

    /// The `NestHost` attribute (Java 11+): the nest host class.
    NestHost { host_class_index: u16 },

    /// The `NestMembers` attribute (Java 11+): classes in this nest.
    NestMembers { classes: Vec<u16> },

    /// The `Record` attribute (Java 16+): record component info.
    Record(Vec<RecordComponent>),

    /// The `PermittedSubclasses` attribute (Java 17+): permitted subclasses of a sealed class.
    PermittedSubclasses { classes: Vec<u16> },

    /// The `Module` attribute (4.7.25, Java 9+): module declaration info.
    Module {
        name_index: u16,
        flags: u16,
        version_index: u16,
        requires: Vec<ModuleRequires>,
        exports: Vec<ModuleExports>,
        opens: Vec<ModuleOpens>,
        uses: Vec<u16>,
        provides: Vec<ModuleProvides>,
    },

    /// The `ModulePackages` attribute (4.7.26, Java 9+): packages in a module.
    ModulePackages { packages: Vec<u16> },

    /// The `ModuleMainClass` attribute (4.7.27, Java 9+): main class of a module.
    ModuleMainClass { main_class_index: u16 },

    /// The `RuntimeVisibleAnnotations` attribute (4.7.16).
    RuntimeVisibleAnnotations(Vec<Annotation>),

    /// The `RuntimeInvisibleAnnotations` attribute (4.7.17).
    RuntimeInvisibleAnnotations(Vec<Annotation>),

    /// The `RuntimeVisibleParameterAnnotations` attribute (4.7.18).
    RuntimeVisibleParameterAnnotations(Vec<Vec<Annotation>>),

    /// The `RuntimeInvisibleParameterAnnotations` attribute (4.7.19).
    RuntimeInvisibleParameterAnnotations(Vec<Vec<Annotation>>),

    /// The `RuntimeVisibleTypeAnnotations` attribute (4.7.20).
    RuntimeVisibleTypeAnnotations(Vec<TypeAnnotation>),

    /// The `RuntimeInvisibleTypeAnnotations` attribute (4.7.21).
    RuntimeInvisibleTypeAnnotations(Vec<TypeAnnotation>),

    /// The `AnnotationDefault` attribute (4.7.22): default value for annotation element.
    AnnotationDefault(ElementValue),

    /// The `LocalVariableTable` attribute (4.7.13): maps local variable slots to source names.
    LocalVariableTable(Vec<LocalVariableEntry>),

    /// The `LocalVariableTypeTable` attribute (4.7.14): generic signatures for local variables.
    LocalVariableTypeTable(Vec<LocalVariableTypeEntry>),

    /// The `MethodParameters` attribute (4.7.24): formal parameter names and access flags.
    MethodParameters(Vec<MethodParameter>),

    /// The `LoadableDescriptors` attribute (Valhalla preview, JEP 401):
    /// list of `CONSTANT_Utf8_info` indices naming field descriptors that the
    /// class loader should eagerly load so that layout decisions (value vs
    /// reference) are stable when this class is linked. Format:
    ///
    /// ```text
    /// LoadableDescriptors_attribute {
    ///     u2 attribute_name_index;
    ///     u4 attribute_length;
    ///     u2 number_of_descriptors;
    ///     u2 descriptors[number_of_descriptors];
    /// }
    /// ```
    LoadableDescriptors { descriptors: Vec<u16> },

    /// An attribute we don't yet parse. Stores the raw bytes.
    Unknown { name: String, data: Vec<u8> },
}

/// The Code attribute structure (JVM spec 4.7.3).
#[derive(Debug, Clone)]
pub struct CodeAttribute {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<u8>,
    pub exception_table: Vec<ExceptionTableEntry>,
    pub attributes: Vec<Attribute>,
}

/// An entry in the exception table of a Code attribute (JVM spec 4.7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionTableEntry {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    /// Index into constant pool for the catch type, or 0 for catch-all (finally).
    pub catch_type: u16,
}

/// An entry in the LineNumberTable attribute (JVM spec 4.7.12).
#[derive(Debug, Clone, Copy)]
pub struct LineNumberEntry {
    pub start_pc: u16,
    pub line_number: u16,
}

/// Inner class information (JVM spec 4.7.6).
#[derive(Debug, Clone)]
pub struct InnerClassInfo {
    pub inner_class_info_index: u16,
    pub outer_class_info_index: u16,
    pub inner_name_index: u16,
    pub inner_class_access_flags: u16,
}

/// A bootstrap method entry (JVM spec 4.7.23).
#[derive(Debug, Clone)]
pub struct BootstrapMethod {
    pub bootstrap_method_ref: u16,
    pub bootstrap_arguments: Vec<u16>,
}

/// A record component (Java 16+).
#[derive(Debug, Clone)]
pub struct RecordComponent {
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<Attribute>,
}

// ---------------------------------------------------------------------------
// Module attribute structures (JVM spec 4.7.25, Java 9+)
// ---------------------------------------------------------------------------

/// A `requires` entry in the Module attribute.
#[derive(Debug, Clone)]
pub struct ModuleRequires {
    pub requires_index: u16,
    pub requires_flags: u16,
    pub requires_version_index: u16,
}

/// An `exports` entry in the Module attribute.
#[derive(Debug, Clone)]
pub struct ModuleExports {
    pub exports_index: u16,
    pub exports_flags: u16,
    pub exports_to: Vec<u16>,
}

/// An `opens` entry in the Module attribute.
#[derive(Debug, Clone)]
pub struct ModuleOpens {
    pub opens_index: u16,
    pub opens_flags: u16,
    pub opens_to: Vec<u16>,
}

/// A `provides` entry in the Module attribute.
#[derive(Debug, Clone)]
pub struct ModuleProvides {
    pub provides_index: u16,
    pub provides_with: Vec<u16>,
}

// ---------------------------------------------------------------------------
// Annotation structures (JVM spec 4.7.16–4.7.24)
// ---------------------------------------------------------------------------

/// A runtime annotation (JVM spec 4.7.16).
#[derive(Debug, Clone)]
pub struct Annotation {
    /// Constant pool index to a Utf8 entry representing the annotation type descriptor.
    pub type_index: u16,
    /// Element-value pairs for this annotation.
    pub element_value_pairs: Vec<ElementValuePair>,
}

/// A name-value pair within an annotation.
#[derive(Debug, Clone)]
pub struct ElementValuePair {
    /// Constant pool index to a Utf8 entry naming the element.
    pub element_name_index: u16,
    /// The value of the element.
    pub value: ElementValue,
}

/// A value within an annotation element (JVM spec 4.7.16.1).
///
/// The tag byte determines which variant is used:
/// - `B`, `C`, `D`, `F`, `I`, `J`, `S`, `Z`, `s` → `Const`
/// - `e` → `Enum`
/// - `c` → `Class`
/// - `@` → `AnnotationValue`
/// - `[` → `Array`
#[derive(Debug, Clone)]
pub enum ElementValue {
    /// Constant value: tag is one of `B/C/D/F/I/J/S/Z/s`.
    /// The `const_value_index` points into the constant pool.
    Const { tag: u8, const_value_index: u16 },
    /// Enum constant: tag is `e`.
    Enum {
        type_name_index: u16,
        const_name_index: u16,
    },
    /// Class literal: tag is `c`.
    Class { class_info_index: u16 },
    /// Nested annotation: tag is `@`.
    AnnotationValue(Annotation),
    /// Array of element values: tag is `[`.
    Array(Vec<ElementValue>),
}

/// A type annotation (JVM spec 4.7.20).
#[derive(Debug, Clone)]
pub struct TypeAnnotation {
    /// The kind of target (JVM spec Table 4.7.20-A/B).
    pub target_type: u8,
    /// Target info — variable-length, stored as raw bytes.
    ///
    /// Intentionally deferred: The 14 different target_info forms in the JVM spec
    /// (Table 4.7.20-A/B) make full structural parsing a poor effort/reward tradeoff
    /// until type annotations are actually consumed (Phase 3 verification).
    /// The raw bytes are validated at read time to have the correct length for each
    /// target_type value (see `read_target_info` in class_reader.rs).
    pub target_info: Vec<u8>,
    /// The type path describing which part of the type is annotated.
    pub type_path: Vec<TypePathEntry>,
    /// The annotation itself.
    pub annotation: Annotation,
}

/// An entry in a type_path structure (JVM spec 4.7.20.2).
#[derive(Debug, Clone, Copy)]
pub struct TypePathEntry {
    /// Kind: 0=array, 1=inner type, 2=wildcard bound, 3=type argument.
    pub type_path_kind: u8,
    /// Which type argument of a parameterized type is annotated (0-based).
    pub type_argument_index: u8,
}

// ---------------------------------------------------------------------------
// LocalVariable / MethodParameters structures
// ---------------------------------------------------------------------------

/// An entry in the `LocalVariableTable` attribute (JVM spec 4.7.13).
#[derive(Debug, Clone, Copy)]
pub struct LocalVariableEntry {
    pub start_pc: u16,
    pub length: u16,
    /// Constant pool index to the variable name (Utf8).
    pub name_index: u16,
    /// Constant pool index to the field descriptor (Utf8).
    pub descriptor_index: u16,
    /// Local variable slot index.
    pub index: u16,
}

/// An entry in the `LocalVariableTypeTable` attribute (JVM spec 4.7.14).
#[derive(Debug, Clone, Copy)]
pub struct LocalVariableTypeEntry {
    pub start_pc: u16,
    pub length: u16,
    /// Constant pool index to the variable name (Utf8).
    pub name_index: u16,
    /// Constant pool index to the field type signature (Utf8).
    pub signature_index: u16,
    /// Local variable slot index.
    pub index: u16,
}

/// A method parameter entry (JVM spec 4.7.24).
#[derive(Debug, Clone, Copy)]
pub struct MethodParameter {
    /// Constant pool index to the parameter name (Utf8), or 0 if unnamed.
    pub name_index: u16,
    /// Access flags: ACC_FINAL (0x0010), ACC_SYNTHETIC (0x1000), ACC_MANDATED (0x8000).
    pub access_flags: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Attribute enum variant construction ──────────────────────────────

    #[test]
    fn attribute_source_file() {
        let attr = Attribute::SourceFile("Main.java".to_string());
        match &attr {
            Attribute::SourceFile(name) => assert_eq!(name, "Main.java"),
            other => panic!("Expected SourceFile, got {other:?}"),
        }
    }

    #[test]
    fn attribute_constant_value() {
        let attr = Attribute::ConstantValue {
            constant_value_index: 42,
        };
        match attr {
            Attribute::ConstantValue {
                constant_value_index,
            } => assert_eq!(constant_value_index, 42),
            other => panic!("Expected ConstantValue, got {other:?}"),
        }
    }

    #[test]
    fn attribute_deprecated_and_synthetic() {
        // Marker attributes with no data
        let dep = Attribute::Deprecated;
        assert!(matches!(dep, Attribute::Deprecated));

        let syn = Attribute::Synthetic;
        assert!(matches!(syn, Attribute::Synthetic));
    }

    #[test]
    fn attribute_exceptions_empty() {
        let attr = Attribute::Exceptions {
            exception_indices: vec![],
        };
        match attr {
            Attribute::Exceptions { exception_indices } => {
                assert!(exception_indices.is_empty());
            }
            other => panic!("Expected Exceptions, got {other:?}"),
        }
    }

    #[test]
    fn attribute_exceptions_multiple() {
        let attr = Attribute::Exceptions {
            exception_indices: vec![1, 2, 3],
        };
        match attr {
            Attribute::Exceptions { exception_indices } => {
                assert_eq!(exception_indices, vec![1, 2, 3]);
            }
            other => panic!("Expected Exceptions, got {other:?}"),
        }
    }

    #[test]
    fn attribute_line_number_table() {
        let entries = vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 1,
            },
            LineNumberEntry {
                start_pc: 10,
                line_number: 5,
            },
        ];
        let attr = Attribute::LineNumberTable(entries);
        match &attr {
            Attribute::LineNumberTable(e) => {
                assert_eq!(e.len(), 2);
                assert_eq!(e[0].start_pc, 0);
                assert_eq!(e[0].line_number, 1);
                assert_eq!(e[1].start_pc, 10);
                assert_eq!(e[1].line_number, 5);
            }
            other => panic!("Expected LineNumberTable, got {other:?}"),
        }
    }

    #[test]
    fn attribute_inner_classes() {
        let info = InnerClassInfo {
            inner_class_info_index: 5,
            outer_class_info_index: 3,
            inner_name_index: 7,
            inner_class_access_flags: 0x0001, // ACC_PUBLIC
        };
        let attr = Attribute::InnerClasses(vec![info]);
        match &attr {
            Attribute::InnerClasses(classes) => {
                assert_eq!(classes.len(), 1);
                assert_eq!(classes[0].inner_class_info_index, 5);
                assert_eq!(classes[0].inner_class_access_flags, 0x0001);
            }
            other => panic!("Expected InnerClasses, got {other:?}"),
        }
    }

    #[test]
    fn attribute_signature() {
        let attr = Attribute::Signature("Ljava/util/List<Ljava/lang/String;>;".to_string());
        match &attr {
            Attribute::Signature(sig) => {
                assert_eq!(sig, "Ljava/util/List<Ljava/lang/String;>;");
            }
            other => panic!("Expected Signature, got {other:?}"),
        }
    }

    #[test]
    fn attribute_stack_map_table_raw_bytes() {
        let attr = Attribute::StackMapTable {
            entries: vec![0x01, 0x02, 0xFF],
        };
        match &attr {
            Attribute::StackMapTable { entries } => {
                assert_eq!(entries, &[0x01, 0x02, 0xFF]);
            }
            other => panic!("Expected StackMapTable, got {other:?}"),
        }
    }

    #[test]
    fn attribute_stack_map_table_empty() {
        let attr = Attribute::StackMapTable {
            entries: vec![],
        };
        match &attr {
            Attribute::StackMapTable { entries } => assert!(entries.is_empty()),
            other => panic!("Expected StackMapTable, got {other:?}"),
        }
    }

    #[test]
    fn attribute_bootstrap_methods() {
        let bm = BootstrapMethod {
            bootstrap_method_ref: 10,
            bootstrap_arguments: vec![20, 30],
        };
        let attr = Attribute::BootstrapMethods(vec![bm]);
        match &attr {
            Attribute::BootstrapMethods(methods) => {
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].bootstrap_method_ref, 10);
                assert_eq!(methods[0].bootstrap_arguments, vec![20, 30]);
            }
            other => panic!("Expected BootstrapMethods, got {other:?}"),
        }
    }

    #[test]
    fn attribute_enclosing_method() {
        let attr = Attribute::EnclosingMethod {
            class_index: 5,
            method_index: 10,
        };
        match attr {
            Attribute::EnclosingMethod {
                class_index,
                method_index,
            } => {
                assert_eq!(class_index, 5);
                assert_eq!(method_index, 10);
            }
            other => panic!("Expected EnclosingMethod, got {other:?}"),
        }
    }

    #[test]
    fn attribute_nest_host_and_members() {
        let host = Attribute::NestHost {
            host_class_index: 42,
        };
        match host {
            Attribute::NestHost { host_class_index } => assert_eq!(host_class_index, 42),
            other => panic!("Expected NestHost, got {other:?}"),
        }

        let members = Attribute::NestMembers {
            classes: vec![1, 2, 3],
        };
        match members {
            Attribute::NestMembers { classes } => assert_eq!(classes, vec![1, 2, 3]),
            other => panic!("Expected NestMembers, got {other:?}"),
        }
    }

    #[test]
    fn attribute_nest_members_empty() {
        let attr = Attribute::NestMembers { classes: vec![] };
        match attr {
            Attribute::NestMembers { classes } => assert!(classes.is_empty()),
            other => panic!("Expected NestMembers, got {other:?}"),
        }
    }

    #[test]
    fn attribute_permitted_subclasses() {
        let attr = Attribute::PermittedSubclasses {
            classes: vec![10, 20],
        };
        match attr {
            Attribute::PermittedSubclasses { classes } => {
                assert_eq!(classes, vec![10, 20]);
            }
            other => panic!("Expected PermittedSubclasses, got {other:?}"),
        }
    }

    #[test]
    fn attribute_module_main_class() {
        let attr = Attribute::ModuleMainClass {
            main_class_index: 99,
        };
        match attr {
            Attribute::ModuleMainClass { main_class_index } => {
                assert_eq!(main_class_index, 99);
            }
            other => panic!("Expected ModuleMainClass, got {other:?}"),
        }
    }

    #[test]
    fn attribute_module_packages() {
        let attr = Attribute::ModulePackages {
            packages: vec![5, 10, 15],
        };
        match attr {
            Attribute::ModulePackages { packages } => {
                assert_eq!(packages, vec![5, 10, 15]);
            }
            other => panic!("Expected ModulePackages, got {other:?}"),
        }
    }

    #[test]
    fn attribute_unknown() {
        let attr = Attribute::Unknown {
            name: "CustomAttr".to_string(),
            data: vec![0xDE, 0xAD],
        };
        match &attr {
            Attribute::Unknown { name, data } => {
                assert_eq!(name, "CustomAttr");
                assert_eq!(data, &[0xDE, 0xAD]);
            }
            other => panic!("Expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn attribute_unknown_empty_data() {
        let attr = Attribute::Unknown {
            name: "Empty".to_string(),
            data: vec![],
        };
        match &attr {
            Attribute::Unknown { name, data } => {
                assert_eq!(name, "Empty");
                assert!(data.is_empty());
            }
            other => panic!("Expected Unknown, got {other:?}"),
        }
    }

    // ── CodeAttribute struct ────────────────────────────────────────────

    #[test]
    fn code_attribute_construction() {
        let code_attr = CodeAttribute {
            max_stack: 4,
            max_locals: 2,
            code: vec![0xB1], // return
            exception_table: vec![],
            attributes: vec![],
        };
        assert_eq!(code_attr.max_stack, 4);
        assert_eq!(code_attr.max_locals, 2);
        assert_eq!(code_attr.code, vec![0xB1]);
        assert!(code_attr.exception_table.is_empty());
        assert!(code_attr.attributes.is_empty());
    }

    #[test]
    fn code_attribute_with_exception_table() {
        let entry = ExceptionTableEntry {
            start_pc: 0,
            end_pc: 10,
            handler_pc: 15,
            catch_type: 5,
        };
        let code_attr = CodeAttribute {
            max_stack: 2,
            max_locals: 1,
            code: vec![],
            exception_table: vec![entry],
            attributes: vec![],
        };
        assert_eq!(code_attr.exception_table.len(), 1);
        assert_eq!(code_attr.exception_table[0].start_pc, 0);
        assert_eq!(code_attr.exception_table[0].end_pc, 10);
        assert_eq!(code_attr.exception_table[0].handler_pc, 15);
        assert_eq!(code_attr.exception_table[0].catch_type, 5);
    }

    #[test]
    fn code_attribute_catch_all_finally() {
        // catch_type=0 means catch-all (finally block)
        let entry = ExceptionTableEntry {
            start_pc: 0,
            end_pc: 20,
            handler_pc: 25,
            catch_type: 0,
        };
        assert_eq!(entry.catch_type, 0);
    }

    #[test]
    fn code_attribute_max_values() {
        let code_attr = CodeAttribute {
            max_stack: u16::MAX,
            max_locals: u16::MAX,
            code: vec![],
            exception_table: vec![],
            attributes: vec![],
        };
        assert_eq!(code_attr.max_stack, u16::MAX);
        assert_eq!(code_attr.max_locals, u16::MAX);
    }

    #[test]
    fn code_attribute_nested_attributes() {
        let inner = Attribute::LineNumberTable(vec![LineNumberEntry {
            start_pc: 0,
            line_number: 1,
        }]);
        let code_attr = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: vec![0xB1],
            exception_table: vec![],
            attributes: vec![inner],
        };
        assert_eq!(code_attr.attributes.len(), 1);
        assert!(matches!(
            &code_attr.attributes[0],
            Attribute::LineNumberTable(_)
        ));
    }

    // ── ExceptionTableEntry struct ──────────────────────────────────────

    #[test]
    fn exception_table_entry_max_values() {
        let entry = ExceptionTableEntry {
            start_pc: u16::MAX,
            end_pc: u16::MAX,
            handler_pc: u16::MAX,
            catch_type: u16::MAX,
        };
        assert_eq!(entry.start_pc, u16::MAX);
        assert_eq!(entry.end_pc, u16::MAX);
        assert_eq!(entry.handler_pc, u16::MAX);
        assert_eq!(entry.catch_type, u16::MAX);
    }

    // ── LineNumberEntry struct ──────────────────────────────────────────

    #[test]
    fn line_number_entry_copy_semantics() {
        let entry = LineNumberEntry {
            start_pc: 5,
            line_number: 42,
        };
        let copy = entry; // Copy trait
        assert_eq!(copy.start_pc, 5);
        assert_eq!(copy.line_number, 42);
        // Original still accessible (Copy, not Move)
        assert_eq!(entry.start_pc, 5);
    }

    // ── BootstrapMethod struct ──────────────────────────────────────────

    #[test]
    fn bootstrap_method_no_arguments() {
        let bm = BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments: vec![],
        };
        assert_eq!(bm.bootstrap_method_ref, 7);
        assert!(bm.bootstrap_arguments.is_empty());
    }

    // ── RecordComponent struct ──────────────────────────────────────────

    #[test]
    fn record_component_construction() {
        let comp = RecordComponent {
            name_index: 3,
            descriptor_index: 5,
            attributes: vec![Attribute::Signature("I".to_string())],
        };
        assert_eq!(comp.name_index, 3);
        assert_eq!(comp.descriptor_index, 5);
        assert_eq!(comp.attributes.len(), 1);
    }

    #[test]
    fn attribute_record_with_components() {
        let comp = RecordComponent {
            name_index: 1,
            descriptor_index: 2,
            attributes: vec![],
        };
        let attr = Attribute::Record(vec![comp]);
        match &attr {
            Attribute::Record(components) => {
                assert_eq!(components.len(), 1);
                assert_eq!(components[0].name_index, 1);
            }
            other => panic!("Expected Record, got {other:?}"),
        }
    }

    // ── Module attribute structures ─────────────────────────────────────

    #[test]
    fn module_requires_construction() {
        let req = ModuleRequires {
            requires_index: 10,
            requires_flags: 0x0020, // ACC_MANDATED
            requires_version_index: 11,
        };
        assert_eq!(req.requires_index, 10);
        assert_eq!(req.requires_flags, 0x0020);
        assert_eq!(req.requires_version_index, 11);
    }

    #[test]
    fn module_exports_with_targets() {
        let exp = ModuleExports {
            exports_index: 5,
            exports_flags: 0,
            exports_to: vec![10, 20],
        };
        assert_eq!(exp.exports_to.len(), 2);
    }

    #[test]
    fn module_opens_empty_targets() {
        let opens = ModuleOpens {
            opens_index: 8,
            opens_flags: 0,
            opens_to: vec![],
        };
        assert!(opens.opens_to.is_empty());
    }

    #[test]
    fn module_provides_construction() {
        let prov = ModuleProvides {
            provides_index: 3,
            provides_with: vec![7, 8, 9],
        };
        assert_eq!(prov.provides_index, 3);
        assert_eq!(prov.provides_with, vec![7, 8, 9]);
    }

    #[test]
    fn attribute_module_full() {
        let attr = Attribute::Module {
            name_index: 1,
            flags: 0x8000,
            version_index: 2,
            requires: vec![ModuleRequires {
                requires_index: 10,
                requires_flags: 0x0020,
                requires_version_index: 0,
            }],
            exports: vec![],
            opens: vec![],
            uses: vec![5, 6],
            provides: vec![ModuleProvides {
                provides_index: 20,
                provides_with: vec![21],
            }],
        };
        match &attr {
            Attribute::Module {
                name_index,
                flags,
                uses,
                provides,
                ..
            } => {
                assert_eq!(*name_index, 1);
                assert_eq!(*flags, 0x8000);
                assert_eq!(uses, &[5, 6]);
                assert_eq!(provides.len(), 1);
            }
            other => panic!("Expected Module, got {other:?}"),
        }
    }

    // ── Annotation structures ───────────────────────────────────────────

    #[test]
    fn annotation_empty_pairs() {
        let ann = Annotation {
            type_index: 5,
            element_value_pairs: vec![],
        };
        assert_eq!(ann.type_index, 5);
        assert!(ann.element_value_pairs.is_empty());
    }

    #[test]
    fn annotation_with_element_value_pairs() {
        let pair = ElementValuePair {
            element_name_index: 10,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: 42,
            },
        };
        let ann = Annotation {
            type_index: 3,
            element_value_pairs: vec![pair],
        };
        assert_eq!(ann.element_value_pairs.len(), 1);
        assert_eq!(ann.element_value_pairs[0].element_name_index, 10);
    }

    #[test]
    fn element_value_all_const_tags() {
        // All valid constant tags: B, C, D, F, I, J, S, Z, s
        for tag in [b'B', b'C', b'D', b'F', b'I', b'J', b'S', b'Z', b's'] {
            let ev = ElementValue::Const {
                tag,
                const_value_index: 1,
            };
            match &ev {
                ElementValue::Const { tag: t, .. } => assert_eq!(*t, tag),
                other => panic!("Expected Const, got {other:?}"),
            }
        }
    }

    #[test]
    fn element_value_enum_variant() {
        let ev = ElementValue::Enum {
            type_name_index: 5,
            const_name_index: 10,
        };
        match ev {
            ElementValue::Enum {
                type_name_index,
                const_name_index,
            } => {
                assert_eq!(type_name_index, 5);
                assert_eq!(const_name_index, 10);
            }
            other => panic!("Expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn element_value_class() {
        let ev = ElementValue::Class {
            class_info_index: 99,
        };
        match ev {
            ElementValue::Class { class_info_index } => assert_eq!(class_info_index, 99),
            other => panic!("Expected Class, got {other:?}"),
        }
    }

    #[test]
    fn element_value_nested_annotation() {
        let inner = Annotation {
            type_index: 7,
            element_value_pairs: vec![],
        };
        let ev = ElementValue::AnnotationValue(inner);
        match &ev {
            ElementValue::AnnotationValue(ann) => assert_eq!(ann.type_index, 7),
            other => panic!("Expected AnnotationValue, got {other:?}"),
        }
    }

    #[test]
    fn element_value_array_empty() {
        let ev = ElementValue::Array(vec![]);
        match &ev {
            ElementValue::Array(arr) => assert!(arr.is_empty()),
            other => panic!("Expected Array, got {other:?}"),
        }
    }

    #[test]
    fn element_value_array_nested() {
        // Array containing another array
        let inner = ElementValue::Array(vec![ElementValue::Const {
            tag: b'I',
            const_value_index: 1,
        }]);
        let outer = ElementValue::Array(vec![inner]);
        match &outer {
            ElementValue::Array(arr) => {
                assert_eq!(arr.len(), 1);
                assert!(matches!(&arr[0], ElementValue::Array(_)));
            }
            other => panic!("Expected Array, got {other:?}"),
        }
    }

    // ── TypeAnnotation structures ───────────────────────────────────────

    #[test]
    fn type_annotation_construction() {
        let ta = TypeAnnotation {
            target_type: 0x13,
            target_info: vec![],
            type_path: vec![TypePathEntry {
                type_path_kind: 3,
                type_argument_index: 0,
            }],
            annotation: Annotation {
                type_index: 5,
                element_value_pairs: vec![],
            },
        };
        assert_eq!(ta.target_type, 0x13);
        assert!(ta.target_info.is_empty());
        assert_eq!(ta.type_path.len(), 1);
        assert_eq!(ta.type_path[0].type_path_kind, 3);
        assert_eq!(ta.annotation.type_index, 5);
    }

    #[test]
    fn type_path_entry_all_kinds() {
        // kind 0=array, 1=inner, 2=wildcard bound, 3=type argument
        for kind in 0..=3u8 {
            let entry = TypePathEntry {
                type_path_kind: kind,
                type_argument_index: 0,
            };
            assert_eq!(entry.type_path_kind, kind);
        }
    }

    #[test]
    fn type_path_entry_copy_semantics() {
        let entry = TypePathEntry {
            type_path_kind: 3,
            type_argument_index: 2,
        };
        let copy = entry; // Copy trait
        assert_eq!(copy.type_path_kind, 3);
        assert_eq!(entry.type_argument_index, 2); // original still valid
    }

    // ── Annotation attribute variants ───────────────────────────────────

    #[test]
    fn attribute_runtime_visible_annotations() {
        let ann = Annotation {
            type_index: 1,
            element_value_pairs: vec![],
        };
        let attr = Attribute::RuntimeVisibleAnnotations(vec![ann]);
        match &attr {
            Attribute::RuntimeVisibleAnnotations(anns) => assert_eq!(anns.len(), 1),
            other => panic!("Expected RuntimeVisibleAnnotations, got {other:?}"),
        }
    }

    #[test]
    fn attribute_runtime_invisible_annotations() {
        let attr = Attribute::RuntimeInvisibleAnnotations(vec![]);
        match &attr {
            Attribute::RuntimeInvisibleAnnotations(anns) => assert!(anns.is_empty()),
            other => panic!("Expected RuntimeInvisibleAnnotations, got {other:?}"),
        }
    }

    #[test]
    fn attribute_runtime_visible_parameter_annotations() {
        let ann = Annotation {
            type_index: 1,
            element_value_pairs: vec![],
        };
        // 2 parameters, first has 1 annotation, second has 0
        let attr = Attribute::RuntimeVisibleParameterAnnotations(vec![vec![ann], vec![]]);
        match &attr {
            Attribute::RuntimeVisibleParameterAnnotations(params) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].len(), 1);
                assert!(params[1].is_empty());
            }
            other => panic!("Expected RuntimeVisibleParameterAnnotations, got {other:?}"),
        }
    }

    #[test]
    fn attribute_runtime_invisible_parameter_annotations() {
        let attr = Attribute::RuntimeInvisibleParameterAnnotations(vec![]);
        assert!(matches!(
            attr,
            Attribute::RuntimeInvisibleParameterAnnotations(_)
        ));
    }

    #[test]
    fn attribute_runtime_visible_type_annotations() {
        let ta = TypeAnnotation {
            target_type: 0x00,
            target_info: vec![0x01],
            type_path: vec![],
            annotation: Annotation {
                type_index: 5,
                element_value_pairs: vec![],
            },
        };
        let attr = Attribute::RuntimeVisibleTypeAnnotations(vec![ta]);
        match &attr {
            Attribute::RuntimeVisibleTypeAnnotations(tas) => {
                assert_eq!(tas.len(), 1);
                assert_eq!(tas[0].target_type, 0x00);
            }
            other => panic!("Expected RuntimeVisibleTypeAnnotations, got {other:?}"),
        }
    }

    #[test]
    fn attribute_runtime_invisible_type_annotations() {
        let attr = Attribute::RuntimeInvisibleTypeAnnotations(vec![]);
        assert!(matches!(
            attr,
            Attribute::RuntimeInvisibleTypeAnnotations(_)
        ));
    }

    #[test]
    fn attribute_annotation_default() {
        let ev = ElementValue::Const {
            tag: b'Z',
            const_value_index: 1,
        };
        let attr = Attribute::AnnotationDefault(ev);
        match &attr {
            Attribute::AnnotationDefault(ElementValue::Const {
                tag,
                const_value_index,
            }) => {
                assert_eq!(*tag, b'Z');
                assert_eq!(*const_value_index, 1);
            }
            other => panic!("Expected AnnotationDefault(Const), got {other:?}"),
        }
    }

    // ── LocalVariable structures ────────────────────────────────────────

    #[test]
    fn local_variable_entry_construction() {
        let entry = LocalVariableEntry {
            start_pc: 0,
            length: 10,
            name_index: 5,
            descriptor_index: 7,
            index: 0,
        };
        assert_eq!(entry.start_pc, 0);
        assert_eq!(entry.length, 10);
        assert_eq!(entry.name_index, 5);
        assert_eq!(entry.descriptor_index, 7);
        assert_eq!(entry.index, 0);
    }

    #[test]
    fn local_variable_entry_copy_semantics() {
        let entry = LocalVariableEntry {
            start_pc: 0,
            length: 5,
            name_index: 1,
            descriptor_index: 2,
            index: 3,
        };
        let copy = entry;
        assert_eq!(copy.index, 3);
        assert_eq!(entry.index, 3); // original still valid
    }

    #[test]
    fn attribute_local_variable_table() {
        let entry = LocalVariableEntry {
            start_pc: 0,
            length: 20,
            name_index: 3,
            descriptor_index: 4,
            index: 1,
        };
        let attr = Attribute::LocalVariableTable(vec![entry]);
        match &attr {
            Attribute::LocalVariableTable(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].index, 1);
            }
            other => panic!("Expected LocalVariableTable, got {other:?}"),
        }
    }

    #[test]
    fn local_variable_type_entry_construction() {
        let entry = LocalVariableTypeEntry {
            start_pc: 0,
            length: 15,
            name_index: 3,
            signature_index: 8,
            index: 2,
        };
        assert_eq!(entry.signature_index, 8);
        assert_eq!(entry.index, 2);
    }

    #[test]
    fn attribute_local_variable_type_table() {
        let entry = LocalVariableTypeEntry {
            start_pc: 0,
            length: 10,
            name_index: 1,
            signature_index: 2,
            index: 0,
        };
        let attr = Attribute::LocalVariableTypeTable(vec![entry]);
        match &attr {
            Attribute::LocalVariableTypeTable(entries) => {
                assert_eq!(entries.len(), 1);
            }
            other => panic!("Expected LocalVariableTypeTable, got {other:?}"),
        }
    }

    // ── MethodParameter struct ──────────────────────────────────────────

    #[test]
    fn method_parameter_construction() {
        let param = MethodParameter {
            name_index: 5,
            access_flags: 0x0010, // ACC_FINAL
        };
        assert_eq!(param.name_index, 5);
        assert_eq!(param.access_flags, 0x0010);
    }

    #[test]
    fn method_parameter_unnamed() {
        // name_index=0 means unnamed
        let param = MethodParameter {
            name_index: 0,
            access_flags: 0,
        };
        assert_eq!(param.name_index, 0);
    }

    #[test]
    fn method_parameter_copy_semantics() {
        let param = MethodParameter {
            name_index: 3,
            access_flags: 0x1000, // ACC_SYNTHETIC
        };
        let copy = param;
        assert_eq!(copy.access_flags, 0x1000);
        assert_eq!(param.access_flags, 0x1000); // original still valid
    }

    #[test]
    fn attribute_method_parameters() {
        let params = vec![
            MethodParameter {
                name_index: 1,
                access_flags: 0x0010,
            },
            MethodParameter {
                name_index: 0,
                access_flags: 0x8000,
            },
        ];
        let attr = Attribute::MethodParameters(params);
        match &attr {
            Attribute::MethodParameters(p) => {
                assert_eq!(p.len(), 2);
                assert_eq!(p[0].access_flags, 0x0010); // ACC_FINAL
                assert_eq!(p[1].access_flags, 0x8000); // ACC_MANDATED
            }
            other => panic!("Expected MethodParameters, got {other:?}"),
        }
    }

    // ── Clone trait ─────────────────────────────────────────────────────

    #[test]
    fn attribute_clone_deep_copy() {
        let original = Attribute::Code(CodeAttribute {
            max_stack: 3,
            max_locals: 2,
            code: vec![0x2A, 0xB7, 0x00, 0x01, 0xB1],
            exception_table: vec![ExceptionTableEntry {
                start_pc: 0,
                end_pc: 5,
                handler_pc: 8,
                catch_type: 3,
            }],
            attributes: vec![Attribute::LineNumberTable(vec![LineNumberEntry {
                start_pc: 0,
                line_number: 1,
            }])],
        });
        let cloned = original.clone();
        match (&original, &cloned) {
            (Attribute::Code(a), Attribute::Code(b)) => {
                assert_eq!(a.max_stack, b.max_stack);
                assert_eq!(a.code, b.code);
                assert_eq!(a.exception_table.len(), b.exception_table.len());
                assert_eq!(a.attributes.len(), b.attributes.len());
            }
            _ => panic!("Clone should preserve variant"),
        }
    }

    #[test]
    fn annotation_clone() {
        let ann = Annotation {
            type_index: 5,
            element_value_pairs: vec![ElementValuePair {
                element_name_index: 10,
                value: ElementValue::Array(vec![
                    ElementValue::Const {
                        tag: b'I',
                        const_value_index: 1,
                    },
                    ElementValue::Const {
                        tag: b'I',
                        const_value_index: 2,
                    },
                ]),
            }],
        };
        let cloned = ann.clone();
        assert_eq!(cloned.type_index, ann.type_index);
        assert_eq!(
            cloned.element_value_pairs.len(),
            ann.element_value_pairs.len()
        );
    }

    // ── Debug trait ─────────────────────────────────────────────────────

    #[test]
    fn attribute_debug_format_not_empty() {
        let attr = Attribute::SourceFile("Test.java".to_string());
        let debug = format!("{attr:?}");
        assert!(debug.contains("SourceFile"));
        assert!(debug.contains("Test.java"));
    }

    #[test]
    fn attribute_debug_deprecated() {
        let attr = Attribute::Deprecated;
        let debug = format!("{attr:?}");
        assert_eq!(debug, "Deprecated");
    }

    #[test]
    fn code_attribute_debug_format() {
        let code = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: vec![0xB1],
            exception_table: vec![],
            attributes: vec![],
        };
        let debug = format!("{code:?}");
        assert!(debug.contains("max_stack: 1"));
        assert!(debug.contains("max_locals: 1"));
    }

    #[test]
    fn element_value_debug_format() {
        let ev = ElementValue::Enum {
            type_name_index: 3,
            const_name_index: 7,
        };
        let debug = format!("{ev:?}");
        assert!(debug.contains("Enum"));
        assert!(debug.contains("3"));
        assert!(debug.contains("7"));
    }

    // ── Edge cases: maximum u16 indices ─────────────────────────────────

    #[test]
    fn constant_value_max_index() {
        let attr = Attribute::ConstantValue {
            constant_value_index: u16::MAX,
        };
        match attr {
            Attribute::ConstantValue {
                constant_value_index,
            } => assert_eq!(constant_value_index, u16::MAX),
            other => panic!("Expected ConstantValue, got {other:?}"),
        }
    }

    #[test]
    fn inner_class_info_max_values() {
        let info = InnerClassInfo {
            inner_class_info_index: u16::MAX,
            outer_class_info_index: u16::MAX,
            inner_name_index: u16::MAX,
            inner_class_access_flags: u16::MAX,
        };
        assert_eq!(info.inner_class_info_index, u16::MAX);
        assert_eq!(info.outer_class_info_index, u16::MAX);
    }

    #[test]
    fn enclosing_method_zero_method_index() {
        // method_index=0 means the class is not enclosed by a method (e.g., field initializer)
        let attr = Attribute::EnclosingMethod {
            class_index: 5,
            method_index: 0,
        };
        match attr {
            Attribute::EnclosingMethod { method_index, .. } => {
                assert_eq!(method_index, 0);
            }
            other => panic!("Expected EnclosingMethod, got {other:?}"),
        }
    }
}
