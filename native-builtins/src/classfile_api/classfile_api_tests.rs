use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_classfile_api_natives(&mut r);
    r
}

// --- Version constants ---

#[test]
fn test_version_constants() {
    assert_eq!(CLASSFILE_MAJOR_45, 45);
    assert_eq!(CLASSFILE_MAJOR_52, 52);
    assert_eq!(CLASSFILE_MAJOR_55, 55);
    assert_eq!(CLASSFILE_MAJOR_61, 61);
    assert_eq!(CLASSFILE_MAJOR_65, 65);
    assert_eq!(CLASSFILE_MAJOR_68, 68);
    assert_eq!(CLASSFILE_MAJOR_69, 69);
}

#[test]
fn test_access_flag_constants() {
    assert_eq!(ACC_PUBLIC, 0x0001);
    assert_eq!(ACC_PRIVATE, 0x0002);
    assert_eq!(ACC_PROTECTED, 0x0004);
    assert_eq!(ACC_STATIC, 0x0008);
    assert_eq!(ACC_FINAL, 0x0010);
    assert_eq!(ACC_SUPER, ACC_SYNCHRONIZED); // both 0x0020
    assert_eq!(ACC_VOLATILE, ACC_BRIDGE); // both 0x0040
    assert_eq!(ACC_TRANSIENT, ACC_VARARGS); // both 0x0080
    assert_eq!(ACC_NATIVE, 0x0100);
    assert_eq!(ACC_INTERFACE, 0x0200);
    assert_eq!(ACC_ABSTRACT, 0x0400);
    assert_eq!(ACC_STRICT, 0x0800);
    assert_eq!(ACC_SYNTHETIC, 0x1000);
    assert_eq!(ACC_ANNOTATION, 0x2000);
    assert_eq!(ACC_ENUM, 0x4000);
    assert_eq!(ACC_MODULE, 0x8000);
}

#[test]
fn test_type_kind_constants() {
    assert_eq!(TK_BYTE, 0);
    assert_eq!(TK_SHORT, 1);
    assert_eq!(TK_INT, 2);
    assert_eq!(TK_LONG, 3);
    assert_eq!(TK_FLOAT, 4);
    assert_eq!(TK_DOUBLE, 5);
    assert_eq!(TK_CHAR, 6);
    assert_eq!(TK_BOOLEAN, 7);
    assert_eq!(TK_REFERENCE, 8);
    assert_eq!(TK_VOID, 9);
}

// --- ClassFile registration ---

#[test]
fn test_classfile_of_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassFile",
            "of",
            "()Ljava/lang/classfile/ClassFile;"
        )
        .is_some());
}

#[test]
fn test_classfile_of_options_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassFile",
            "of",
            "([Ljava/lang/classfile/ClassFile$Option;)Ljava/lang/classfile/ClassFile;"
        )
        .is_some());
}

#[test]
fn test_classfile_parse_bytes_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassFile",
            "parse",
            "([B)Ljava/lang/classfile/ClassModel;"
        )
        .is_some());
}

#[test]
fn test_classfile_parse_path_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassFile",
            "parse",
            "(Ljava/nio/file/Path;)Ljava/lang/classfile/ClassModel;"
        )
        .is_some());
}

#[test]
fn test_classfile_build_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassFile",
            "build",
            "(Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)[B"
        )
        .is_some());
}

#[test]
fn test_classfile_latest_major_version_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/ClassFile", "latestMajorVersion", "()I")
        .is_some());
}

#[test]
fn test_classfile_latest_minor_version_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/ClassFile", "latestMinorVersion", "()I")
        .is_some());
}

#[test]
fn test_classfile_transform_class_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassFile",
            "transformClass",
            "(Ljava/lang/classfile/ClassModel;Ljava/lang/classfile/ClassTransform;)[B"
        )
        .is_some());
}

// --- ClassModel registration ---

#[test]
fn test_class_model_major_version_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/ClassModel", "majorVersion", "()I")
        .is_some());
}

#[test]
fn test_class_model_minor_version_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/ClassModel", "minorVersion", "()I")
        .is_some());
}

#[test]
fn test_class_model_flags_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassModel",
            "flags",
            "()Ljava/lang/classfile/AccessFlags;"
        )
        .is_some());
}

#[test]
fn test_class_model_this_class_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassModel",
            "thisClass",
            "()Ljava/lang/classfile/constantpool/ClassEntry;"
        )
        .is_some());
}

#[test]
fn test_class_model_superclass_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassModel",
            "superclass",
            "()Ljava/util/Optional;"
        )
        .is_some());
}

#[test]
fn test_class_model_is_module_info_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/ClassModel", "isModuleInfo", "()Z")
        .is_some());
}

#[test]
fn test_class_model_constant_pool_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassModel",
            "constantPool",
            "()Ljava/lang/classfile/constantpool/ConstantPool;"
        )
        .is_some());
}

// --- MethodModel registration ---

#[test]
fn test_method_model_flags_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/MethodModel",
            "flags",
            "()Ljava/lang/classfile/AccessFlags;"
        )
        .is_some());
}

#[test]
fn test_method_model_method_name_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/MethodModel",
            "methodName",
            "()Ljava/lang/classfile/constantpool/Utf8Entry;"
        )
        .is_some());
}

#[test]
fn test_method_model_code_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/MethodModel",
            "code",
            "()Ljava/util/Optional;"
        )
        .is_some());
}

// --- FieldModel registration ---

#[test]
fn test_field_model_flags_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/FieldModel",
            "flags",
            "()Ljava/lang/classfile/AccessFlags;"
        )
        .is_some());
}

#[test]
fn test_field_model_field_name_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/FieldModel",
            "fieldName",
            "()Ljava/lang/classfile/constantpool/Utf8Entry;"
        )
        .is_some());
}

#[test]
fn test_field_model_attributes_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/FieldModel",
            "attributes",
            "()Ljava/util/List;"
        )
        .is_some());
}

// --- CodeModel registration ---

#[test]
fn test_code_model_max_stack_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/CodeModel", "maxStack", "()I")
        .is_some());
}

#[test]
fn test_code_model_max_locals_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/CodeModel", "maxLocals", "()I")
        .is_some());
}

#[test]
fn test_code_model_code_length_registered() {
    let r = make_registry();
    assert!(r
        .find("java/lang/classfile/CodeModel", "codeLength", "()I")
        .is_some());
}

#[test]
fn test_code_model_exception_handlers_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeModel",
            "exceptionHandlers",
            "()Ljava/util/List;"
        )
        .is_some());
}

// --- ClassBuilder registration ---

#[test]
fn test_class_builder_with_flags_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassBuilder",
            "withFlags",
            "(I)Ljava/lang/classfile/ClassBuilder;"
        )
        .is_some());
}

#[test]
fn test_class_builder_with_field_consumer_registered() {
    let r = make_registry();
    assert!(r.find("java/lang/classfile/ClassBuilder", "withField",
        "(Ljava/lang/String;Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;").is_some());
}

#[test]
fn test_class_builder_with_method_registered() {
    let r = make_registry();
    assert!(r.find("java/lang/classfile/ClassBuilder", "withMethod",
        "(Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;ILjava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;").is_some());
}

// --- CodeBuilder registration ---

#[test]
fn test_code_builder_aload_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "aload",
            "(I)Ljava/lang/classfile/CodeBuilder;"
        )
        .is_some());
}

#[test]
fn test_code_builder_iconst_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "iconst",
            "(I)Ljava/lang/classfile/CodeBuilder;"
        )
        .is_some());
}

#[test]
fn test_code_builder_iadd_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "iadd",
            "()Ljava/lang/classfile/CodeBuilder;"
        )
        .is_some());
}

#[test]
fn test_code_builder_invokevirtual_registered() {
    let r = make_registry();
    assert!(r.find("java/lang/classfile/CodeBuilder", "invokevirtual",
        "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;)Ljava/lang/classfile/CodeBuilder;").is_some());
}

#[test]
fn test_code_builder_new_label_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "newLabel",
            "()Ljava/lang/classfile/Label;"
        )
        .is_some());
}

#[test]
fn test_code_builder_goto_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "goto_",
            "(Ljava/lang/classfile/Label;)Ljava/lang/classfile/CodeBuilder;"
        )
        .is_some());
}

#[test]
fn test_code_builder_return_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "return_",
            "()Ljava/lang/classfile/CodeBuilder;"
        )
        .is_some());
}

#[test]
fn test_code_builder_getfield_registered() {
    let r = make_registry();
    assert!(r.find("java/lang/classfile/CodeBuilder", "getfield",
        "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;").is_some());
}

#[test]
fn test_code_builder_block_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeBuilder",
            "block",
            "(Ljava/util/function/Consumer;)Ljava/lang/classfile/CodeBuilder;"
        )
        .is_some());
}

// --- ClassTransform registration ---

#[test]
fn test_class_transform_of_stateful_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassTransform",
            "ofStateful",
            "(Ljava/util/function/Supplier;)Ljava/lang/classfile/ClassTransform;"
        )
        .is_some());
}

#[test]
fn test_class_transform_dropping_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassTransform",
            "dropping",
            "(Ljava/util/function/Predicate;)Ljava/lang/classfile/ClassTransform;"
        )
        .is_some());
}

#[test]
fn test_class_transform_accept_all_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/ClassTransform",
            "ACCEPT_ALL",
            "()Ljava/lang/classfile/ClassTransform;"
        )
        .is_some());
}

// --- CodeTransform registration ---

#[test]
fn test_code_transform_of_stateful_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeTransform",
            "ofStateful",
            "(Ljava/util/function/Supplier;)Ljava/lang/classfile/CodeTransform;"
        )
        .is_some());
}

#[test]
fn test_code_transform_and_then_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeTransform",
            "andThen",
            "(Ljava/lang/classfile/CodeTransform;)Ljava/lang/classfile/CodeTransform;"
        )
        .is_some());
}

#[test]
fn test_code_transform_accept_all_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/CodeTransform",
            "ACCEPT_ALL",
            "()Ljava/lang/classfile/CodeTransform;"
        )
        .is_some());
}

// --- Attribute registration ---

#[test]
fn test_attribute_name_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/Attribute",
            "attributeName",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn test_attribute_mapper_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/Attribute",
            "attributeMapper",
            "()Ljava/lang/classfile/AttributeMapper;"
        )
        .is_some());
}

// --- ConstantPool registration ---

#[test]
fn test_constant_pool_entry_count_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/constantpool/ConstantPool",
            "entryCount",
            "()I"
        )
        .is_some());
}

#[test]
fn test_constant_pool_size_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/constantpool/ConstantPool",
            "size",
            "()I"
        )
        .is_some());
}

#[test]
fn test_constant_pool_bootstrap_method_count_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "java/lang/classfile/constantpool/ConstantPool",
            "bootstrapMethodCount",
            "()I"
        )
        .is_some());
}
