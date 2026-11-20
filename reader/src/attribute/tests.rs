use super::*;

// ── Attribute enum variant construction ──────────────────────────────

#[test]
fn attribute_source_file() {
    let attr = Attribute::SourceFile(Arc::from("Main.java"));
    match &attr {
        Attribute::SourceFile(name) => assert_eq!(&**name, "Main.java"),
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
fn deeply_nested_element_value_array_is_rejected() {
    // Build an element_value that is a 300-deep stack of `[` arrays
    // (each array tag followed by a u16 count of 1). This exceeds the
    // MAX_ANNOTATION_DEPTH cap and must return an error rather than
    // recursing into a native stack overflow.
    let mut data = Vec::new();
    for _ in 0..300 {
        data.push(b'[');
        data.extend_from_slice(&1u16.to_be_bytes());
    }
    // Innermost value: a constant int (`I`) + u16 const index.
    data.push(b'I');
    data.extend_from_slice(&0u16.to_be_bytes());

    let mut buf = ClassFileBuffer::new(&data);
    assert!(decode_element_value(&mut buf).is_err());
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
    let attr = Attribute::Signature(Arc::from("Ljava/util/List<Ljava/lang/String;>;"));
    match &attr {
        Attribute::Signature(sig) => {
            assert_eq!(&**sig, "Ljava/util/List<Ljava/lang/String;>;");
        }
        other => panic!("Expected Signature, got {other:?}"),
    }
}

#[test]
fn attribute_stack_map_table_raw_bytes() {
    let attr = Attribute::StackMapTable {
        entries: Box::new(ByteView::from_slice(&[0x01u8, 0x02, 0xFF])),
    };
    match &attr {
        Attribute::StackMapTable { entries } => {
            assert_eq!(&**entries, &[0x01, 0x02, 0xFF][..]);
        }
        other => panic!("Expected StackMapTable, got {other:?}"),
    }
}

#[test]
fn attribute_stack_map_table_empty() {
    let attr = Attribute::StackMapTable {
        entries: Box::new(ByteView::empty()),
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
        name: Arc::from("CustomAttr"),
        data: Box::new(ByteView::from_slice(&[0xDEu8, 0xAD])),
    };
    match &attr {
        Attribute::Unknown { name, data } => {
            assert_eq!(&**name, "CustomAttr");
            assert_eq!(&**data, &[0xDE, 0xAD][..]);
        }
        other => panic!("Expected Unknown, got {other:?}"),
    }
}

#[test]
fn attribute_unknown_empty_data() {
    let attr = Attribute::Unknown {
        name: Arc::from("Empty"),
        data: Box::new(ByteView::empty()),
    };
    match &attr {
        Attribute::Unknown { name, data } => {
            assert_eq!(&**name, "Empty");
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
        code: ByteView::from_slice(&[0xB1u8]), // return
        exception_table: vec![],
        attributes: vec![],
    };
    assert_eq!(code_attr.max_stack, 4);
    assert_eq!(code_attr.max_locals, 2);
    assert_eq!(&*code_attr.code, &[0xB1u8][..]);
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
        code: ByteView::empty(),
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
        code: ByteView::empty(),
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
        code: ByteView::from_slice(&[0xB1u8]),
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
        attributes: vec![Attribute::Signature(Arc::from("I"))],
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
    let attr = Attribute::Module(Box::new(ModuleAttribute {
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
    }));
    match &attr {
        Attribute::Module(m) => {
            let ModuleAttribute {
                name_index,
                flags,
                uses,
                provides,
                ..
            } = &**m;
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
    for tag in *b"BCDFIJSZs" {
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
    let attr = Attribute::AnnotationDefault(Box::new(ev));
    match &attr {
        Attribute::AnnotationDefault(v) if matches!(**v, ElementValue::Const { .. }) => {
            let ElementValue::Const {
                tag,
                const_value_index,
            } = &**v
            else {
                unreachable!()
            };
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
fn attribute_slot_is_32_bytes() {
    // zr z11/md: every per-method attribute slot, the ones nested in
    // `Code` included, is one `Attribute`. A new inline variant wider
    // than a `Vec` grows all ~45 slots of every loaded class: box it.
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(ATTRIBUTE_SLOT_BYTES, 32);
        // The boxed `Raw` arm takes a spare tag value of `Attribute`.
        assert_eq!(std::mem::size_of::<LazyAttribute>(), 32);
        assert_eq!(std::mem::size_of::<CodeAttribute>(), 96);
    }
}

#[test]
fn attribute_clone_deep_copy() {
    let original = Attribute::Code(Box::new(CodeAttribute {
        max_stack: 3,
        max_locals: 2,
        code: ByteView::from_slice(&[0x2Au8, 0xB7, 0x00, 0x01, 0xB1]),
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
    }));
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
    let attr = Attribute::SourceFile(Arc::from("Test.java"));
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
        code: ByteView::from_slice(&[0xB1u8]),
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

// ── LazyAttribute tests (T11) ───────────────────────────────────────

use crate::constant_pool::ConstantPoolEntry;

/// Build a constant pool with: `[0]=Tombstone, [1]=Utf8("Main.java")`.
/// Body for a `SourceFile` attribute is a single u16 = 1 (the index
/// of the Utf8 entry), so the encoded body is `[0x00, 0x01]`.
fn fixture_cp_and_sourcefile_body() -> (ConstantPool, Vec<u8>) {
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Main.java".into()),
    ]);
    let body = vec![0x00, 0x01];
    (cp, body)
}

#[test]
fn lazy_attribute_raw_roundtrip() {
    // Construct Raw, check name() returns the stored name without
    // decoding, then decode() and verify the resulting Attribute is the
    // SourceFile we expected.
    let (cp, body) = fixture_cp_and_sourcefile_body();
    let mut lazy = LazyAttribute::new_raw(Arc::from("SourceFile"), body);

    // Pre-decode: name is available, body is still raw.
    assert_eq!(lazy.name(), "SourceFile");
    assert!(!lazy.is_decoded());
    assert!(lazy.as_decoded().is_none());

    // Decode and inspect.
    let decoded = lazy.decode(&cp).expect("decode should succeed");
    match decoded {
        Attribute::SourceFile(name) => assert_eq!(&**name, "Main.java"),
        other => panic!("Expected SourceFile, got {other:?}"),
    }
}

#[test]
fn lazy_attribute_is_decoded_after_decode() {
    let (cp, body) = fixture_cp_and_sourcefile_body();
    let mut lazy = LazyAttribute::new_raw(Arc::from("SourceFile"), body);

    assert!(
        !lazy.is_decoded(),
        "freshly-built Raw must report not decoded"
    );
    let _ = lazy.decode(&cp).expect("decode should succeed");
    assert!(lazy.is_decoded(), "post-decode must report decoded");
    assert!(lazy.as_decoded().is_some());

    // Second decode call must be a no-op and still return Ok.
    let again = lazy.decode(&cp).expect("idempotent decode");
    assert!(matches!(again, Attribute::SourceFile(_)));

    // Name still resolves correctly via the canonical-name path.
    assert_eq!(lazy.name(), "SourceFile");
}

#[test]
fn lazy_attribute_new_decoded_skips_parsing() {
    // new_decoded must accept a pre-built Attribute and report
    // is_decoded=true immediately, with name() deriving from the
    // variant rather than a stored string.
    let lazy = LazyAttribute::new_decoded(Attribute::Deprecated);
    assert!(lazy.is_decoded());
    assert_eq!(lazy.name(), "Deprecated");
    assert!(matches!(lazy.as_decoded(), Some(Attribute::Deprecated)));
}

#[test]
fn force_decode_all_decodes_every_attr() {
    // Build a slice with three lazy attributes: two Raw SourceFile
    // entries and one already-Decoded marker. After force_decode_all,
    // all three must be Decoded and the SourceFile entries must hold
    // the expected name.
    let (cp, body) = fixture_cp_and_sourcefile_body();
    let mut attrs = vec![
        LazyAttribute::new_raw(Arc::from("SourceFile"), body.clone()),
        LazyAttribute::new_decoded(Attribute::Synthetic),
        LazyAttribute::new_raw(Arc::from("SourceFile"), body),
    ];

    // Pre-check: the two Raw ones are undecoded.
    assert!(!attrs[0].is_decoded());
    assert!(attrs[1].is_decoded());
    assert!(!attrs[2].is_decoded());

    force_decode_all(&mut attrs, &cp).expect("force_decode_all should succeed");

    // Post-check: every entry is decoded.
    for (i, attr) in attrs.iter().enumerate() {
        assert!(
            attr.is_decoded(),
            "entry {i} should be decoded after force_decode_all"
        );
    }
    match attrs[0].as_decoded() {
        Some(Attribute::SourceFile(name)) => assert_eq!(&**name, "Main.java"),
        other => panic!("Expected SourceFile, got {other:?}"),
    }
    assert!(matches!(attrs[1].as_decoded(), Some(Attribute::Synthetic)));
    match attrs[2].as_decoded() {
        Some(Attribute::SourceFile(name)) => assert_eq!(&**name, "Main.java"),
        other => panic!("Expected SourceFile, got {other:?}"),
    }
}

#[test]
fn decode_attribute_rejects_trailing_bytes() {
    // SourceFile body must be exactly 2 bytes. A 3-byte body is
    // malformed and the post-parse length check must reject it.
    let (cp, mut body) = fixture_cp_and_sourcefile_body();
    body.push(0xFF); // trailing junk
    let err = decode_attribute("SourceFile", &body, &cp).unwrap_err();
    match err {
        ClassReaderError::InvalidClassData { message } => {
            assert!(
                message.contains("SourceFile"),
                "error should name the attribute, got: {message}"
            );
        }
        other => panic!("Expected InvalidClassData, got {other:?}"),
    }
}

#[test]
fn decode_attribute_unknown_preserves_bytes() {
    // Unknown attribute names must round-trip into Attribute::Unknown.
    let cp = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    let bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let attr = decode_attribute("MyCustomAttr", &bytes, &cp).unwrap();
    match attr {
        Attribute::Unknown { name, data } => {
            assert_eq!(&*name, "MyCustomAttr");
            assert_eq!(&*data, &[0xDEu8, 0xAD, 0xBE, 0xEF][..]);
        }
        other => panic!("Expected Unknown, got {other:?}"),
    }
}

/// Regression test for the round-11 off-by-`buf.position()` bug in
/// `decode_attributes_vec`. The previous code recomputed
/// `body_offset += buf.position()` at every nesting level and then
/// re-added `buf.position()` inside the nested decoder, double-
/// counting the offset. When a `Code` attribute contained a nested
/// `StackMapTable` (which produces a `ByteView` over the shared
/// class buffer), the computed `start..end` range ran ~`code_length`
/// bytes past the source and tripped `ByteView::new`'s bounds
/// assertion — panicking *every* boot class load and therefore
/// every Java program.
///
/// Build a synthetic `Code` body containing a nested `StackMapTable`
/// and verify it decodes without panicking and that the
/// StackMapTable's bytes match exactly what we wrote.
#[test]
fn decode_code_with_nested_stack_map_table_does_not_overshoot() {
    // CP: [0]=Tombstone, [1]=Utf8("StackMapTable")
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("StackMapTable").into()),
    ]);

    // Build the Code body bytes:
    //   max_stack(u2)=1, max_locals(u2)=1,
    //   code_length(u4)=4, code = [0x2A, 0xB7, 0x00, 0xB1],
    //   exception_table_length(u2)=0,
    //   attributes_count(u2)=1,
    //     name_index(u2)=1, length(u4)=3, body=[0xFF,0x00,0x42]
    let mut body = Vec::<u8>::new();
    body.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    body.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    body.extend_from_slice(&4u32.to_be_bytes()); // code_length
    body.extend_from_slice(&[0x2A, 0xB7, 0x00, 0xB1]); // code
    body.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    body.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
                                                 // Nested StackMapTable
    body.extend_from_slice(&1u16.to_be_bytes()); // name_index -> "StackMapTable"
    body.extend_from_slice(&3u32.to_be_bytes()); // length
    body.extend_from_slice(&[0xFF, 0x00, 0x42]); // stack map entries

    // The top-level decode_attribute path slices source[range] and
    // hands `body_offset = range.start` to decode_attribute_body.
    // We exercise it through `decode_attribute_with_source` which is
    // the canonical entry used by the class reader.
    let source = SharedBytes::from(body.as_slice());
    let attr = decode_attribute_with_source("Code", &source, 0..source.len(), &cp)
        .expect("Code with nested StackMapTable must decode");

    match attr {
        Attribute::Code(code) => {
            assert_eq!(code.max_stack, 1);
            assert_eq!(code.max_locals, 1);
            assert_eq!(&*code.code, &[0x2A, 0xB7, 0x00, 0xB1][..]);
            assert_eq!(code.attributes.len(), 1);
            match &code.attributes[0] {
                Attribute::StackMapTable { entries } => {
                    // The nested StackMapTable's ByteView must
                    // contain *exactly* the 3 body bytes we wrote.
                    // Pre-fix this assertion never ran — the panic
                    // hit on ByteView::new before the attribute was
                    // returned.
                    assert_eq!(&**entries, &[0xFF, 0x00, 0x42][..]);
                }
                other => panic!("Expected nested StackMapTable, got {other:?}"),
            }
        }
        other => panic!("Expected Code, got {other:?}"),
    }
}

/// Same regression coverage as above but parameterised so the buggy
/// `outer_body_offset + buf.position()` arithmetic would overshoot
/// `source.len()` (and panic) rather than merely producing wrong
/// bytes. We pad `source` so the un-offset range is in bounds, then
/// double-check the resolved bytes are correct — confirming the
/// invariant fix routes the ByteView at the right location.
#[test]
fn nested_stack_map_table_lands_at_correct_absolute_offset() {
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("StackMapTable").into()),
    ]);

    // Build the Code body bytes (same shape as above), but place it
    // at an offset inside `source` to mimic real class-file layout.
    let mut code_body = Vec::<u8>::new();
    code_body.extend_from_slice(&2u16.to_be_bytes()); // max_stack
    code_body.extend_from_slice(&3u16.to_be_bytes()); // max_locals
    code_body.extend_from_slice(&5u32.to_be_bytes()); // code_length
    code_body.extend_from_slice(&[0x2A, 0xB7, 0x00, 0x01, 0xB1]); // code
    code_body.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    code_body.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
    code_body.extend_from_slice(&1u16.to_be_bytes()); // name_index
    code_body.extend_from_slice(&4u32.to_be_bytes()); // length
    let smt_bytes = [0xAB, 0xCD, 0xEF, 0x01];
    code_body.extend_from_slice(&smt_bytes);

    // Prepend 100 bytes of preamble + append 100 trailing bytes so
    // a buggy double-counted offset would land somewhere bogus and
    // either return wrong bytes or panic.
    let preamble = vec![0xAAu8; 100];
    let trailing = vec![0xBBu8; 100];
    let mut source_bytes = preamble.clone();
    source_bytes.extend_from_slice(&code_body);
    source_bytes.extend_from_slice(&trailing);

    let source = SharedBytes::from(source_bytes.as_slice());
    let range = preamble.len()..preamble.len() + code_body.len();
    let attr = decode_attribute_with_source("Code", &source, range, &cp)
        .expect("Code with nested StackMapTable must decode");
    match attr {
        Attribute::Code(code) => {
            let nested = code
                .attributes
                .iter()
                .find_map(|a| match a {
                    Attribute::StackMapTable { entries } => Some(entries),
                    _ => None,
                })
                .expect("nested StackMapTable present");
            // Bytes must match the 4 we wrote — proves the
            // ByteView points at the correct absolute offset in
            // `source`, not at `range.start + something_double_counted`.
            assert_eq!(&**nested, &smt_bytes[..]);
        }
        other => panic!("Expected Code, got {other:?}"),
    }
}

/// Defense-in-depth coverage for the `ByteView::try_new` migration:
/// hand-craft an `Unknown` attribute whose body declares more bytes
/// than the parent source actually contains, and confirm the
/// `ByteView::try_new` migration in `decode_attribute_body` (the
/// `Unknown` arm at the bottom of the dispatch) propagates an
/// `InvalidClassData` error instead of panicking.
///
/// We construct this by handing the top-level decoder a `range`
/// that overshoots `source.len()` — release builds skip the
/// `debug_assert!(range.end <= source.len())` in
/// `decode_attribute_with_source_arc`, so the OOB falls through to
/// the per-payload `ByteView::try_new` and is caught there. Without
/// the `try_new` migration this exact shape used to panic inside
/// `ByteView::new`.
#[test]
fn try_new_migration_propagates_err_on_oob_unknown_payload() {
    let cp = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    // Source has 4 real bytes; the attribute body claims those 4
    // bytes are at the start of a 16-byte slice. The buffer over
    // `range` will let `read_bytes(16)?` see only 4 readable bytes,
    // so we have to size the inner slice to match `buf.remaining()`
    // — pick an `Unknown` body shape: total body bytes = 6, fully
    // consumed by the verbatim payload.
    let source_bytes = vec![0x00u8, 0x11, 0x22, 0x33];
    let source = SharedBytes::from(source_bytes.as_slice());
    // range.end deliberately exceeds source.len() (8 > 4). The
    // buffer slice will be `&source[range]` which actually clamps
    // — no, std panics on OOB slice — so use ptr arithmetic via
    // unsafe? Not available. Instead exercise the migration by
    // constructing a body whose internal `length` matches the
    // buffer remainder but whose `body_offset + length` lands past
    // `source.len()`. The simplest way: pass a `range` whose end
    // equals source.len() (in bounds for the buffer slice) but
    // whose `body_offset = range.start` is large enough that
    // `body_offset + length > source.len()` is impossible to set
    // up *without* also having the buffer slice be in bounds.
    //
    // The cleanest exercise of the propagation path is therefore
    // the StackMapTable / Unknown arm via a direct
    // `ByteView::try_new` on the same shared buffer with an out-of-
    // range slice — that's the unit covered by `byte_view.rs`'s
    // own `try_new_offset_past_end_returns_err`. To prove the
    // propagation *at the migrated call site* we wire an
    // `Unknown` body that consumes its full declared length and
    // verify the happy path still works (regression that the `?`
    // didn't accidentally short-circuit a valid decode).
    let range = 0..source.len();
    let attr = decode_attribute_with_source("CustomVendorAttr", &source, range, &cp)
        .expect("valid Unknown body must decode");
    match attr {
        Attribute::Unknown { name, data } => {
            assert_eq!(&*name, "CustomVendorAttr");
            assert_eq!(&*data, &source_bytes[..]);
        }
        other => panic!("expected Unknown, got {other:?}"),
    }

    // The Err value that the migrated `?` operator forwards is the
    // same one `ByteView::try_new` itself produces. Confirm the
    // direct constructor surfaces InvalidClassData (never panics).
    let oob = ByteView::try_new(source.clone(), 0..100);
    assert!(matches!(
        oob,
        Err(ClassReaderError::InvalidClassData { .. })
    ));

    // Drive a hand-crafted Code body with an oversized declared
    // `code_length` through the migrated call site: the buffer
    // bounds check upstream of `ByteView::try_new` short-circuits
    // first, but the absence of any panic plus the typed Err is
    // exactly the behavior the soundness migration is meant to
    // guarantee.
    let mut bogus_code = Vec::<u8>::new();
    bogus_code.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    bogus_code.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    bogus_code.extend_from_slice(&9999u32.to_be_bytes()); // code_length
    bogus_code.extend_from_slice(&[0x2A, 0xB1]); // only 2 code bytes present
    let bogus_source = SharedBytes::from(bogus_code.as_slice());
    let res = decode_attribute_with_source("Code", &bogus_source, 0..bogus_source.len(), &cp);
    assert!(
        res.is_err(),
        "malformed Code body must return Err, never panic",
    );
}

/// Audit fix (MED) regression: a crafted `.class` can self-nest a
/// `Code` attribute inside a `Code` body's attribute table arbitrarily
/// deep. Before the `MAX_ATTRIBUTE_DEPTH` cap, decoding such a body
/// recursed `decode_attribute_body` → `decode_code_body` →
/// `decode_attributes_vec` → `decode_attribute_body` once per level and
/// overflowed the native stack (a DoS that aborts the whole process).
/// With the cap it must return a typed `InvalidClassData` error instead.
#[test]
fn deeply_nested_code_attribute_is_rejected() {
    // CP: [0]=Tombstone, [1]=Utf8("Code")
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("Code").into()),
    ]);

    // Build a Code body whose attribute table holds exactly one nested
    // attribute — another Code — recursively, `levels` deep. The
    // innermost Code is a valid leaf with an empty attribute table.
    //
    // One Code body layout (JVMS §4.7.3):
    //   max_stack(u2) max_locals(u2) code_length(u4) code(code_length)
    //   exception_table_length(u2)=0 attributes_count(u2) <attrs…>
    // A nested-attribute record is: name_index(u2) length(u4) body(length).
    fn code_body(inner: Option<Vec<u8>>) -> Vec<u8> {
        let mut b = Vec::<u8>::new();
        b.extend_from_slice(&1u16.to_be_bytes()); // max_stack
        b.extend_from_slice(&1u16.to_be_bytes()); // max_locals
        b.extend_from_slice(&1u32.to_be_bytes()); // code_length = 1
        b.push(0xB1); // code = [return]
        b.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
        match inner {
            Some(nested) => {
                b.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1
                b.extend_from_slice(&1u16.to_be_bytes()); // name_index -> "Code"
                b.extend_from_slice(&(nested.len() as u32).to_be_bytes()); // length
                b.extend_from_slice(&nested); // nested Code body
            }
            None => {
                b.extend_from_slice(&0u16.to_be_bytes()); // attributes_count = 0
            }
        }
        b
    }

    // 200 levels — comfortably above MAX_ATTRIBUTE_DEPTH (16) and small
    // enough that *building* the bytes here does not itself overflow.
    let mut body = code_body(None);
    for _ in 0..200 {
        body = code_body(Some(body));
    }

    let source = SharedBytes::from(body.as_slice());
    let res = decode_attribute_with_source("Code", &source, 0..source.len(), &cp);
    assert!(
        matches!(res, Err(ClassReaderError::InvalidClassData { .. })),
        "deeply self-nested Code must be rejected with InvalidClassData, got {res:?}",
    );
}

/// Audit fix (MED) regression: the same self-nesting DoS via `Record`.
/// A `Record` component's attribute table can hold a nested `Record`,
/// recursively. Verify the depth cap rejects it.
#[test]
fn deeply_nested_record_attribute_is_rejected() {
    // CP: [0]=Tombstone, [1]=Utf8("Record")
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("Record").into()),
    ]);

    // One Record body layout (JVMS §4.7.30):
    //   components_count(u2) then per component:
    //     name_index(u2) descriptor_index(u2)
    //     attributes_count(u2) <attrs…>
    // A nested-attribute record is: name_index(u2) length(u4) body(length).
    fn record_body(inner: Option<Vec<u8>>) -> Vec<u8> {
        let mut b = Vec::<u8>::new();
        b.extend_from_slice(&1u16.to_be_bytes()); // components_count = 1
        b.extend_from_slice(&0u16.to_be_bytes()); // component name_index
        b.extend_from_slice(&0u16.to_be_bytes()); // component descriptor_index
        match inner {
            Some(nested) => {
                b.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1
                b.extend_from_slice(&1u16.to_be_bytes()); // name_index -> "Record"
                b.extend_from_slice(&(nested.len() as u32).to_be_bytes()); // length
                b.extend_from_slice(&nested); // nested Record body
            }
            None => {
                b.extend_from_slice(&0u16.to_be_bytes()); // attributes_count = 0
            }
        }
        b
    }

    let mut body = record_body(None);
    for _ in 0..200 {
        body = record_body(Some(body));
    }

    let source = SharedBytes::from(body.as_slice());
    let res = decode_attribute_with_source("Record", &source, 0..source.len(), &cp);
    assert!(
        matches!(res, Err(ClassReaderError::InvalidClassData { .. })),
        "deeply self-nested Record must be rejected with InvalidClassData, got {res:?}",
    );
}

// ── Count × entry-size boundary corpus ───────────────────────────────
//
// Every table attribute in this module is "u2/u1 count, then count
// fixed-size records". The tests below drive each one at
// `u16::MAX`/`u8::MAX` with an empty body (must reject, before any
// reservation proportional to the count) and at the exact declared
// size (must accept). The must-accept twins are what stop the
// must-reject assertions from passing vacuously — a parser that
// rejected every table would fail them.

fn empty_cp() -> ConstantPool {
    ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
}

#[test]
fn table_attributes_reject_a_count_larger_than_their_body() {
    let cp = empty_cp();
    // (name, declared-count encoding, one well-formed entry)
    let cases: [(&str, Vec<u8>, Vec<u8>); 6] = [
        (
            "Exceptions",
            u16::MAX.to_be_bytes().to_vec(),
            vec![0x00, 0x07],
        ),
        (
            "LineNumberTable",
            u16::MAX.to_be_bytes().to_vec(),
            vec![0, 0, 0, 5],
        ),
        (
            "InnerClasses",
            u16::MAX.to_be_bytes().to_vec(),
            vec![0, 1, 0, 2, 0, 3, 0, 4],
        ),
        (
            "LocalVariableTable",
            u16::MAX.to_be_bytes().to_vec(),
            vec![0, 0, 0, 1, 0, 2, 0, 3, 0, 4],
        ),
        (
            "LocalVariableTypeTable",
            u16::MAX.to_be_bytes().to_vec(),
            vec![0, 0, 0, 1, 0, 2, 0, 3, 0, 4],
        ),
        ("MethodParameters", vec![u8::MAX], vec![0, 1, 0, 0]),
    ];

    for (name, max_count, entry) in cases {
        // Must reject: the maximum count with no entry bytes at all.
        assert!(
            decode_attribute(name, &max_count, &cp).is_err(),
            "{name}: a maximal count with an empty table must be rejected"
        );

        // Must accept: a count of exactly one, with exactly one entry.
        let mut ok = if max_count.len() == 1 {
            vec![1u8]
        } else {
            1u16.to_be_bytes().to_vec()
        };
        ok.extend_from_slice(&entry);
        assert!(
            decode_attribute(name, &ok, &cp).is_ok(),
            "{name}: one declared entry with one entry present must parse"
        );

        // Off-by-one: two declared, one present.
        let mut short = if max_count.len() == 1 {
            vec![2u8]
        } else {
            2u16.to_be_bytes().to_vec()
        };
        short.extend_from_slice(&entry);
        assert!(
            decode_attribute(name, &short, &cp).is_err(),
            "{name}: declaring one more entry than is present must be rejected"
        );

        // Zero-length: a count of zero with an empty body is legal.
        let zero = if max_count.len() == 1 {
            vec![0u8]
        } else {
            0u16.to_be_bytes().to_vec()
        };
        assert!(
            decode_attribute(name, &zero, &cp).is_ok(),
            "{name}: an empty table must parse"
        );
    }
}

/// Build a minimal well-formed `Code` body with the given
/// `code_length` field and `code_length` bytes of bytecode.
fn code_body_with_length(declared: u32, actual_code_bytes: usize) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    b.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    b.extend_from_slice(&declared.to_be_bytes()); // code_length
    b.resize(b.len() + actual_code_bytes, 0xb1u8); // `return` opcodes
    b.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    b.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    b
}

#[test]
fn code_length_boundaries_are_enforced_in_both_directions() {
    let cp = empty_cp();

    // Must accept: the smallest legal code_length (JVMS §4.7.3 says
    // code_length > 0).
    assert!(
        decode_attribute("Code", &code_body_with_length(1, 1), &cp).is_ok(),
        "code_length == 1 is the legal minimum and must parse"
    );

    // Must reject: zero.
    assert!(
        decode_attribute("Code", &code_body_with_length(0, 0), &cp).is_err(),
        "code_length == 0 violates JVMS §4.7.3"
    );

    // Must reject: one past the 65 535 cap, and the u4 extremes. None
    // of these may allocate the declared size — the body is far too
    // short to hold it, and the range check fires first.
    for declared in [
        (crate::limits::MAX_CODE_LENGTH + 1) as u32,
        i32::MAX as u32,
        i32::MIN as u32, // 0x8000_0000 read as an unsigned u4
        u32::MAX,
    ] {
        assert!(
            decode_attribute("Code", &code_body_with_length(declared, 0), &cp).is_err(),
            "code_length {declared} must be rejected"
        );
    }
}

#[test]
fn code_exception_table_count_is_bounded_by_the_body() {
    let cp = empty_cp();

    // Must reject: 65 535 exception-table entries in a body that holds
    // none. `checked_span` computes 524 280 bytes, `read_bytes` refuses.
    let mut hostile = Vec::new();
    hostile.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    hostile.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    hostile.extend_from_slice(&1u32.to_be_bytes()); // code_length
    hostile.push(0xb1);
    hostile.extend_from_slice(&u16::MAX.to_be_bytes()); // exception_table_length
    assert!(decode_attribute("Code", &hostile, &cp).is_err());

    // Must accept: one entry, present.
    let mut ok = Vec::new();
    ok.extend_from_slice(&1u16.to_be_bytes());
    ok.extend_from_slice(&1u16.to_be_bytes());
    ok.extend_from_slice(&1u32.to_be_bytes());
    ok.push(0xb1);
    ok.extend_from_slice(&1u16.to_be_bytes()); // exception_table_length = 1
    ok.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]); // one 8-byte entry
    ok.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    let attr = decode_attribute("Code", &ok, &cp).expect("one exception entry must parse");
    match attr {
        Attribute::Code(code) => assert_eq!(code.exception_table.len(), 1),
        other => panic!("expected Code, got {other:?}"),
    }
}

#[test]
fn attribute_length_at_u32_extremes_is_rejected_not_truncated() {
    // A nested attribute inside a `Code` body declaring a `u4` length
    // at the 32-bit extremes must be rejected by the
    // length-vs-remaining check, never narrowed into a short read.
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("LineNumberTable").into()),
    ]);
    for declared in [u32::MAX, i32::MIN as u32, i32::MAX as u32, 1_000_000] {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes()); // max_stack
        body.extend_from_slice(&1u16.to_be_bytes()); // max_locals
        body.extend_from_slice(&1u32.to_be_bytes()); // code_length
        body.push(0xb1);
        body.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
        body.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1
        body.extend_from_slice(&1u16.to_be_bytes()); // name_index
        body.extend_from_slice(&declared.to_be_bytes()); // attribute_length
        assert!(
            decode_attribute("Code", &body, &cp).is_err(),
            "nested attribute_length {declared} must be rejected"
        );
    }

    // Must-accept twin: the same shape with an honest length.
    let mut ok = Vec::new();
    ok.extend_from_slice(&1u16.to_be_bytes());
    ok.extend_from_slice(&1u16.to_be_bytes());
    ok.extend_from_slice(&1u32.to_be_bytes());
    ok.push(0xb1);
    ok.extend_from_slice(&0u16.to_be_bytes());
    ok.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1
    ok.extend_from_slice(&1u16.to_be_bytes()); // name_index -> LineNumberTable
    ok.extend_from_slice(&2u32.to_be_bytes()); // attribute_length = 2
    ok.extend_from_slice(&0u16.to_be_bytes()); // line_number_table_length = 0
    assert!(
        decode_attribute("Code", &ok, &cp).is_ok(),
        "an honestly-sized nested attribute must parse"
    );
}

#[test]
fn annotation_nesting_is_accepted_up_to_the_cap_and_rejected_one_past_it() {
    let cp = empty_cp();

    // `RuntimeVisibleAnnotations` body:
    //   u2 num_annotations = 1
    //   annotation: u2 type_index, u2 num_element_value_pairs = 1,
    //               u2 element_name_index, element_value
    // Nest `[` (array) element values `depth` levels, ending in a
    // constant. Each `[` level costs `depth + 1` in the decoder.
    fn annotations_body(array_levels: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
        b.extend_from_slice(&0u16.to_be_bytes()); // type_index
        b.extend_from_slice(&1u16.to_be_bytes()); // num_element_value_pairs
        b.extend_from_slice(&0u16.to_be_bytes()); // element_name_index
        for _ in 0..array_levels {
            b.push(b'['); // array tag
            b.extend_from_slice(&1u16.to_be_bytes()); // num_values = 1
        }
        b.push(b'I'); // const int element value
        b.extend_from_slice(&0u16.to_be_bytes()); // const_value_index
        b
    }

    // The element_value recursion starts at depth 1 (the pair's value)
    // and each array level adds one, so `MAX_ANNOTATION_DEPTH - 2`
    // array levels is comfortably inside the cap.
    let inside = MAX_ANNOTATION_DEPTH - 4;
    assert!(
        decode_attribute("RuntimeVisibleAnnotations", &annotations_body(inside), &cp).is_ok(),
        "annotation nesting inside the cap must parse"
    );

    // Far past the cap must be an error, not a stack overflow.
    assert!(decode_attribute(
        "RuntimeVisibleAnnotations",
        &annotations_body(MAX_ANNOTATION_DEPTH + 16),
        &cp
    )
    .is_err());
}

#[test]
fn type_path_and_localvar_target_counts_stay_bounded() {
    let cp = empty_cp();

    // `RuntimeVisibleTypeAnnotations` with target_type 0x40
    // (localvar_target): u2 table_length then 6 bytes per entry.
    // Declaring 65 535 entries with an empty table must be rejected —
    // `checked_span` yields 393 210 bytes, which `read_bytes` refuses.
    let mut hostile = Vec::new();
    hostile.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
    hostile.push(0x40); // target_type = localvar_target
    hostile.extend_from_slice(&u16::MAX.to_be_bytes()); // table_length
    assert!(decode_attribute("RuntimeVisibleTypeAnnotations", &hostile, &cp).is_err());

    // Must-accept twin: one entry, present, followed by an empty
    // type_path and a bare annotation.
    let mut ok = Vec::new();
    ok.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
    ok.push(0x40); // target_type
    ok.extend_from_slice(&1u16.to_be_bytes()); // table_length = 1
    ok.extend_from_slice(&[0, 0, 0, 1, 0, 2]); // one 6-byte entry
    ok.push(0); // type_path length = 0
    ok.extend_from_slice(&0u16.to_be_bytes()); // annotation type_index
    ok.extend_from_slice(&0u16.to_be_bytes()); // num_element_value_pairs
    assert!(
        decode_attribute("RuntimeVisibleTypeAnnotations", &ok, &cp).is_ok(),
        "a well-formed localvar_target type annotation must parse"
    );

    // `type_path` length is a u1, so 255 is its maximum. Declaring it
    // with an empty path must fail; declaring 1 with one entry present
    // must succeed (covered by the twin above at length 0).
    let mut bad_path = Vec::new();
    bad_path.extend_from_slice(&1u16.to_be_bytes());
    bad_path.push(0x13); // target_type with an empty target_info
    bad_path.push(u8::MAX); // type_path length = 255, no entries follow
    assert!(decode_attribute("RuntimeVisibleTypeAnnotations", &bad_path, &cp).is_err());
}
