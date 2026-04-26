//! Class file reader — parses raw bytes into a [`ClassFile`] structure.
//!
//! Reference: <https://docs.oracle.com/javase/specs/jvms/se21/html/jvms-4.html>

use crate::attribute::*;
use crate::buffer::ClassFileBuffer;
use crate::class_access_flags::*;
use crate::class_file::ClassFile;
use crate::class_file_version::ClassFileVersion;
use crate::class_reader_error::ClassReaderError;
use crate::constant_pool::{ConstantPool, ConstantPoolEntry};
use crate::field::ClassFileField;
use crate::method::ClassFileMethod;
use tracing::{debug, trace};

const CLASS_FILE_MAGIC: u32 = 0xCAFEBABE;

/// Safety cap for `Vec::with_capacity` to avoid excessive pre-allocation on
/// malformed class files.  The actual count (bounded by u16) may exceed this;
/// the Vec will simply grow on demand.
const PREALLOC_CAP: usize = 1024;

/// Maximum count for constant pool, methods, fields, interfaces, exception
/// table entries, and attributes.  Per JVM spec these are u16 fields, so the
/// hard upper bound is 65 535.  We validate against this limit *before*
/// allocating to prevent a crafted class file with huge counts from causing
/// an out-of-memory denial-of-service.
const MAX_CP_SIZE: u16 = u16::MAX;       // 65 535 — JVM spec §4.1
const MAX_FIELD_COUNT: u16 = u16::MAX;
const MAX_METHOD_COUNT: u16 = u16::MAX;
const MAX_INTERFACE_COUNT: u16 = u16::MAX;
const MAX_ATTRIBUTE_COUNT: u16 = u16::MAX;
const MAX_EXCEPTION_TABLE_COUNT: u16 = u16::MAX;

/// Validate that a section count does not exceed the given limit.
fn validate_count(label: &str, count: u16, limit: u16) -> Result<(), ClassReaderError> {
    if count > limit {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "{label} count {count} exceeds maximum allowed value {limit}"
            ),
        });
    }
    Ok(())
}

/// Parse a `.class` file from a byte slice.
pub fn read_class(data: &[u8]) -> Result<ClassFile, ClassReaderError> {
    let mut buf = ClassFileBuffer::new(data);

    // Magic number
    let magic = buf.read_u32()?;
    if magic != CLASS_FILE_MAGIC {
        return Err(ClassReaderError::InvalidMagicNumber { magic });
    }

    // Version
    let minor = buf.read_u16()?;
    let major = buf.read_u16()?;
    let version = ClassFileVersion::new(major, minor);
    if !version.is_supported() {
        return Err(ClassReaderError::UnsupportedVersion { major, minor });
    }
    debug!("Class file version: {version}");

    // Constant pool
    let constant_pool = read_constant_pool(&mut buf)?;
    debug!("Constant pool: {} entries", constant_pool.len());

    // Access flags, this class, super class
    let access_flags_raw = buf.read_u16()?;
    let access_flags = ClassAccessFlags::from_bits_truncate(access_flags_raw);

    let this_class_index = buf.read_u16()?;
    let this_class = constant_pool
        .get_class_name(this_class_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: this_class_index,
            message: "this_class must reference a valid Class entry".to_string(),
        })?
        .to_string();
    debug!("Parsing class: {this_class}");

    let super_class_index = buf.read_u16()?;
    let super_class = if super_class_index == 0 {
        None // java.lang.Object has no superclass
    } else {
        Some(
            constant_pool
                .get_class_name(super_class_index)
                .ok_or_else(|| ClassReaderError::InvalidConstantPool {
                    index: super_class_index,
                    message: "super_class must reference a valid Class entry".to_string(),
                })?
                .to_string(),
        )
    };

    // Interfaces
    let interfaces_count = buf.read_u16()?;
    validate_count("interfaces", interfaces_count, MAX_INTERFACE_COUNT)?;
    let mut interfaces = Vec::with_capacity((interfaces_count as usize).min(PREALLOC_CAP));
    for _ in 0..interfaces_count {
        let iface_index = buf.read_u16()?;
        let iface_name = constant_pool.get_class_name(iface_index).ok_or_else(|| {
            ClassReaderError::InvalidConstantPool {
                index: iface_index,
                message: "interface must reference a valid Class entry".to_string(),
            }
        })?;
        interfaces.push(iface_name.to_string());
    }

    // Fields
    let fields_count = buf.read_u16()?;
    validate_count("fields", fields_count, MAX_FIELD_COUNT)?;
    let mut fields = Vec::with_capacity((fields_count as usize).min(PREALLOC_CAP));
    for _ in 0..fields_count {
        fields.push(read_field(&mut buf, &constant_pool)?);
    }

    // Methods
    let methods_count = buf.read_u16()?;
    validate_count("methods", methods_count, MAX_METHOD_COUNT)?;
    let mut methods = Vec::with_capacity((methods_count as usize).min(PREALLOC_CAP));
    for _ in 0..methods_count {
        methods.push(read_method(&mut buf, &constant_pool)?);
    }

    // Class attributes
    let attributes = read_attributes(&mut buf, &constant_pool)?;

    Ok(ClassFile {
        version,
        constant_pool,
        access_flags,
        this_class,
        super_class,
        interfaces,
        fields,
        methods,
        attributes,
    })
}

fn read_constant_pool(buf: &mut ClassFileBuffer) -> Result<ConstantPool, ClassReaderError> {
    let count = buf.read_u16()?;
    validate_count("constant_pool", count, MAX_CP_SIZE)?;
    let mut entries: Vec<ConstantPoolEntry> = Vec::with_capacity((count as usize).min(PREALLOC_CAP));
    entries.push(ConstantPoolEntry::Tombstone); // Index 0

    let mut i = 1u16;
    while i < count {
        let tag = buf.read_u8()?;
        let entry = match tag {
            1 => {
                // CONSTANT_Utf8 — length is u16, so max 65535 bytes; safe.
                //
                // Hot-path interning: every CONSTANT_Utf8 entry is funneled
                // through the global string pool. Identical UTF-8 content
                // loaded via different `.class` files therefore shares a
                // single `Arc<str>` allocation — 100s of class files that
                // reference `"java/lang/Object"` in their constant pools
                // end up with one backing allocation for that name, with
                // each `Arc<str>` clone being a single refcount bump.
                let length = buf.read_u16()?;
                let bytes = buf.read_bytes(length as usize)?;
                let string = cesu8::from_java_cesu8(bytes)
                    .map_err(|_| ClassReaderError::InvalidCesu8String { index: i })?;
                ConstantPoolEntry::Utf8(rustjvm_types::intern_arc(&string))
            }
            3 => {
                // CONSTANT_Integer
                let value = buf.read_i32()?;
                ConstantPoolEntry::Integer(value)
            }
            4 => {
                // CONSTANT_Float
                let value = buf.read_f32()?;
                ConstantPoolEntry::Float(value)
            }
            5 => {
                // CONSTANT_Long (takes 2 slots)
                let value = buf.read_i64()?;
                entries.push(ConstantPoolEntry::Long(value));
                entries.push(ConstantPoolEntry::Tombstone);
                i += 2;
                continue;
            }
            6 => {
                // CONSTANT_Double (takes 2 slots)
                let value = buf.read_f64()?;
                entries.push(ConstantPoolEntry::Double(value));
                entries.push(ConstantPoolEntry::Tombstone);
                i += 2;
                continue;
            }
            7 => {
                // CONSTANT_Class
                let name_index = buf.read_u16()?;
                ConstantPoolEntry::ClassReference { name_index }
            }
            8 => {
                // CONSTANT_String
                let string_index = buf.read_u16()?;
                ConstantPoolEntry::StringReference { string_index }
            }
            9 => {
                // CONSTANT_Fieldref
                let class_index = buf.read_u16()?;
                let name_and_type_index = buf.read_u16()?;
                ConstantPoolEntry::FieldReference {
                    class_index,
                    name_and_type_index,
                }
            }
            10 => {
                // CONSTANT_Methodref
                let class_index = buf.read_u16()?;
                let name_and_type_index = buf.read_u16()?;
                ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                }
            }
            11 => {
                // CONSTANT_InterfaceMethodref
                let class_index = buf.read_u16()?;
                let name_and_type_index = buf.read_u16()?;
                ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                }
            }
            12 => {
                // CONSTANT_NameAndType
                let name_index = buf.read_u16()?;
                let descriptor_index = buf.read_u16()?;
                ConstantPoolEntry::NameAndType {
                    name_index,
                    descriptor_index,
                }
            }
            15 => {
                // CONSTANT_MethodHandle
                let reference_kind = buf.read_u8()?;
                let reference_index = buf.read_u16()?;
                ConstantPoolEntry::MethodHandle {
                    reference_kind,
                    reference_index,
                }
            }
            16 => {
                // CONSTANT_MethodType
                let descriptor_index = buf.read_u16()?;
                ConstantPoolEntry::MethodType { descriptor_index }
            }
            17 => {
                // CONSTANT_Dynamic (Java 11+)
                let bootstrap_method_attr_index = buf.read_u16()?;
                let name_and_type_index = buf.read_u16()?;
                ConstantPoolEntry::Dynamic {
                    bootstrap_method_attr_index,
                    name_and_type_index,
                }
            }
            18 => {
                // CONSTANT_InvokeDynamic
                let bootstrap_method_attr_index = buf.read_u16()?;
                let name_and_type_index = buf.read_u16()?;
                ConstantPoolEntry::InvokeDynamic {
                    bootstrap_method_attr_index,
                    name_and_type_index,
                }
            }
            19 => {
                // CONSTANT_Module (Java 9+)
                let name_index = buf.read_u16()?;
                ConstantPoolEntry::Module { name_index }
            }
            20 => {
                // CONSTANT_Package (Java 9+)
                let name_index = buf.read_u16()?;
                ConstantPoolEntry::Package { name_index }
            }
            _ => {
                return Err(ClassReaderError::InvalidConstantPoolTag { index: i, tag });
            }
        };

        entries.push(entry);
        i += 1;
    }

    Ok(ConstantPool::new(entries))
}

fn read_field(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
) -> Result<ClassFileField, ClassReaderError> {
    let access_flags_raw = buf.read_u16()?;
    let access_flags = FieldAccessFlags::from_bits_truncate(access_flags_raw);
    let name_index = buf.read_u16()?;
    // Fetch the `Arc<str>` straight from the constant pool — it was already
    // interned at parse time via `rustjvm_types::intern_arc`, so this is a
    // single refcount bump (no allocation, no UTF-8 re-copy).
    let name = constant_pool
        .get_utf8_arc(name_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: name_index,
            message: "field name must reference a valid Utf8 entry".to_string(),
        })?;
    let descriptor_index = buf.read_u16()?;
    let descriptor = constant_pool
        .get_utf8_arc(descriptor_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: descriptor_index,
            message: "field descriptor must reference a valid Utf8 entry".to_string(),
        })?;
    let attributes = read_attributes(buf, constant_pool)?;
    trace!("  Field: {name}: {descriptor}");

    Ok(ClassFileField {
        access_flags,
        name,
        descriptor,
        attributes,
    })
}

fn read_method(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
) -> Result<ClassFileMethod, ClassReaderError> {
    let access_flags_raw = buf.read_u16()?;
    let access_flags = MethodAccessFlags::from_bits_truncate(access_flags_raw);
    let name_index = buf.read_u16()?;
    // Fetch the `Arc<str>` straight from the constant pool — it was already
    // interned at parse time via `rustjvm_types::intern_arc`, so this is a
    // single refcount bump (no allocation, no UTF-8 re-copy).
    let name = constant_pool
        .get_utf8_arc(name_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: name_index,
            message: "method name must reference a valid Utf8 entry".to_string(),
        })?;
    let descriptor_index = buf.read_u16()?;
    let descriptor = constant_pool
        .get_utf8_arc(descriptor_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: descriptor_index,
            message: "method descriptor must reference a valid Utf8 entry".to_string(),
        })?;
    let attributes = read_attributes(buf, constant_pool)?;
    trace!("  Method: {name}{descriptor}");

    Ok(ClassFileMethod {
        access_flags,
        name,
        descriptor,
        attributes,
    })
}

fn read_attributes(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
) -> Result<Vec<Attribute>, ClassReaderError> {
    let count = buf.read_u16()?;
    validate_count("attributes", count, MAX_ATTRIBUTE_COUNT)?;
    let mut attributes = Vec::with_capacity((count as usize).min(PREALLOC_CAP));

    for _ in 0..count {
        let name_index = buf.read_u16()?;
        let name = constant_pool.get_utf8(name_index).ok_or_else(|| {
            ClassReaderError::InvalidConstantPool {
                index: name_index,
                message: "attribute name must reference a valid Utf8 entry".to_string(),
            }
        })?;
        let length = buf.read_u32()? as usize;

        // Validate attribute length does not exceed remaining buffer
        if length > buf.remaining() {
            return Err(ClassReaderError::InvalidClassData {
                message: format!(
                    "attribute '{name}' length {length} exceeds remaining buffer size {}",
                    buf.remaining()
                ),
            });
        }

        let attr = match name {
            "Code" => read_code_attribute(buf, constant_pool)?,
            "SourceFile" => {
                let source_file_index = buf.read_u16()?;
                let source_file = constant_pool
                    .get_utf8(source_file_index)
                    .ok_or_else(|| ClassReaderError::InvalidConstantPool {
                        index: source_file_index,
                        message: "SourceFile must reference a valid Utf8 entry".to_string(),
                    })?
                    .to_string();
                Attribute::SourceFile(source_file)
            }
            "ConstantValue" => {
                let constant_value_index = buf.read_u16()?;
                Attribute::ConstantValue {
                    constant_value_index,
                }
            }
            "Deprecated" => Attribute::Deprecated,
            "Synthetic" => Attribute::Synthetic,
            "Exceptions" => {
                let num_exceptions = buf.read_u16()?;
                let mut exception_indices = Vec::with_capacity((num_exceptions as usize).min(PREALLOC_CAP));
                for _ in 0..num_exceptions {
                    exception_indices.push(buf.read_u16()?);
                }
                Attribute::Exceptions { exception_indices }
            }
            "LineNumberTable" => {
                let table_length = buf.read_u16()?;
                let mut entries = Vec::with_capacity((table_length as usize).min(PREALLOC_CAP));
                for _ in 0..table_length {
                    entries.push(LineNumberEntry {
                        start_pc: buf.read_u16()?,
                        line_number: buf.read_u16()?,
                    });
                }
                Attribute::LineNumberTable(entries)
            }
            "InnerClasses" => {
                let num_classes = buf.read_u16()?;
                let mut classes = Vec::with_capacity((num_classes as usize).min(PREALLOC_CAP));
                for _ in 0..num_classes {
                    classes.push(InnerClassInfo {
                        inner_class_info_index: buf.read_u16()?,
                        outer_class_info_index: buf.read_u16()?,
                        inner_name_index: buf.read_u16()?,
                        inner_class_access_flags: buf.read_u16()?,
                    });
                }
                Attribute::InnerClasses(classes)
            }
            "Signature" => {
                let signature_index = buf.read_u16()?;
                let signature = constant_pool
                    .get_utf8(signature_index)
                    .ok_or_else(|| ClassReaderError::InvalidConstantPool {
                        index: signature_index,
                        message: "Signature must reference a valid Utf8 entry".to_string(),
                    })?
                    .to_string();
                Attribute::Signature(signature)
            }
            "StackMapTable" => {
                let data = buf.read_bytes(length)?.to_vec();
                Attribute::StackMapTable { entries: data }
            }
            "BootstrapMethods" => {
                let num_bootstrap_methods = buf.read_u16()?;
                let mut methods =
                    Vec::with_capacity((num_bootstrap_methods as usize).min(PREALLOC_CAP));
                for _ in 0..num_bootstrap_methods {
                    let bootstrap_method_ref = buf.read_u16()?;
                    let num_args = buf.read_u16()?;
                    let mut bootstrap_arguments = Vec::with_capacity((num_args as usize).min(PREALLOC_CAP));
                    for _ in 0..num_args {
                        bootstrap_arguments.push(buf.read_u16()?);
                    }
                    methods.push(BootstrapMethod {
                        bootstrap_method_ref,
                        bootstrap_arguments,
                    });
                }
                Attribute::BootstrapMethods(methods)
            }
            "EnclosingMethod" => Attribute::EnclosingMethod {
                class_index: buf.read_u16()?,
                method_index: buf.read_u16()?,
            },
            "NestHost" => Attribute::NestHost {
                host_class_index: buf.read_u16()?,
            },
            "NestMembers" => {
                let num = buf.read_u16()?;
                let mut classes = Vec::with_capacity((num as usize).min(PREALLOC_CAP));
                for _ in 0..num {
                    classes.push(buf.read_u16()?);
                }
                Attribute::NestMembers { classes }
            }
            "Record" => {
                let num_components = buf.read_u16()?;
                let mut components =
                    Vec::with_capacity((num_components as usize).min(PREALLOC_CAP));
                for _ in 0..num_components {
                    let comp_name_index = buf.read_u16()?;
                    let comp_descriptor_index = buf.read_u16()?;
                    let comp_attributes = read_attributes(buf, constant_pool)?;
                    components.push(RecordComponent {
                        name_index: comp_name_index,
                        descriptor_index: comp_descriptor_index,
                        attributes: comp_attributes,
                    });
                }
                Attribute::Record(components)
            }
            "PermittedSubclasses" => {
                let num = buf.read_u16()?;
                let mut classes = Vec::with_capacity((num as usize).min(PREALLOC_CAP));
                for _ in 0..num {
                    classes.push(buf.read_u16()?);
                }
                Attribute::PermittedSubclasses { classes }
            }
            "Module" => {
                let name_index = buf.read_u16()?;
                let flags = buf.read_u16()?;
                let version_index = buf.read_u16()?;

                let requires_count = buf.read_u16()?;
                let mut requires = Vec::with_capacity((requires_count as usize).min(PREALLOC_CAP));
                for _ in 0..requires_count {
                    requires.push(ModuleRequires {
                        requires_index: buf.read_u16()?,
                        requires_flags: buf.read_u16()?,
                        requires_version_index: buf.read_u16()?,
                    });
                }

                let exports_count = buf.read_u16()?;
                let mut exports = Vec::with_capacity((exports_count as usize).min(PREALLOC_CAP));
                for _ in 0..exports_count {
                    let exports_index = buf.read_u16()?;
                    let exports_flags = buf.read_u16()?;
                    let to_count = buf.read_u16()?;
                    let mut exports_to = Vec::with_capacity((to_count as usize).min(PREALLOC_CAP));
                    for _ in 0..to_count {
                        exports_to.push(buf.read_u16()?);
                    }
                    exports.push(ModuleExports {
                        exports_index,
                        exports_flags,
                        exports_to,
                    });
                }

                let opens_count = buf.read_u16()?;
                let mut opens = Vec::with_capacity((opens_count as usize).min(PREALLOC_CAP));
                for _ in 0..opens_count {
                    let opens_index = buf.read_u16()?;
                    let opens_flags = buf.read_u16()?;
                    let to_count = buf.read_u16()?;
                    let mut opens_to = Vec::with_capacity((to_count as usize).min(PREALLOC_CAP));
                    for _ in 0..to_count {
                        opens_to.push(buf.read_u16()?);
                    }
                    opens.push(ModuleOpens {
                        opens_index,
                        opens_flags,
                        opens_to,
                    });
                }

                let uses_count = buf.read_u16()?;
                let mut uses = Vec::with_capacity((uses_count as usize).min(PREALLOC_CAP));
                for _ in 0..uses_count {
                    uses.push(buf.read_u16()?);
                }

                let provides_count = buf.read_u16()?;
                let mut provides = Vec::with_capacity((provides_count as usize).min(PREALLOC_CAP));
                for _ in 0..provides_count {
                    let provides_index = buf.read_u16()?;
                    let with_count = buf.read_u16()?;
                    let mut provides_with = Vec::with_capacity((with_count as usize).min(PREALLOC_CAP));
                    for _ in 0..with_count {
                        provides_with.push(buf.read_u16()?);
                    }
                    provides.push(ModuleProvides {
                        provides_index,
                        provides_with,
                    });
                }

                Attribute::Module {
                    name_index,
                    flags,
                    version_index,
                    requires,
                    exports,
                    opens,
                    uses,
                    provides,
                }
            }
            "ModulePackages" => {
                let count = buf.read_u16()?;
                let mut packages = Vec::with_capacity((count as usize).min(PREALLOC_CAP));
                for _ in 0..count {
                    packages.push(buf.read_u16()?);
                }
                Attribute::ModulePackages { packages }
            }
            "ModuleMainClass" => Attribute::ModuleMainClass {
                main_class_index: buf.read_u16()?,
            },
            "RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations" => {
                let num_annotations = buf.read_u16()?;
                let mut annotations = Vec::with_capacity((num_annotations as usize).min(PREALLOC_CAP));
                for _ in 0..num_annotations {
                    annotations.push(read_annotation(buf)?);
                }
                if name == "RuntimeVisibleAnnotations" {
                    Attribute::RuntimeVisibleAnnotations(annotations)
                } else {
                    Attribute::RuntimeInvisibleAnnotations(annotations)
                }
            }
            "RuntimeVisibleParameterAnnotations" | "RuntimeInvisibleParameterAnnotations" => {
                let num_parameters = buf.read_u8()?;
                let mut parameter_annotations = Vec::with_capacity((num_parameters as usize).min(PREALLOC_CAP));
                for _ in 0..num_parameters {
                    let num_annotations = buf.read_u16()?;
                    let mut annotations = Vec::with_capacity((num_annotations as usize).min(PREALLOC_CAP));
                    for _ in 0..num_annotations {
                        annotations.push(read_annotation(buf)?);
                    }
                    parameter_annotations.push(annotations);
                }
                if name == "RuntimeVisibleParameterAnnotations" {
                    Attribute::RuntimeVisibleParameterAnnotations(parameter_annotations)
                } else {
                    Attribute::RuntimeInvisibleParameterAnnotations(parameter_annotations)
                }
            }
            "RuntimeVisibleTypeAnnotations" | "RuntimeInvisibleTypeAnnotations" => {
                let num_annotations = buf.read_u16()?;
                let mut annotations = Vec::with_capacity((num_annotations as usize).min(PREALLOC_CAP));
                for _ in 0..num_annotations {
                    annotations.push(read_type_annotation(buf)?);
                }
                if name == "RuntimeVisibleTypeAnnotations" {
                    Attribute::RuntimeVisibleTypeAnnotations(annotations)
                } else {
                    Attribute::RuntimeInvisibleTypeAnnotations(annotations)
                }
            }
            "AnnotationDefault" => {
                let value = read_element_value(buf)?;
                Attribute::AnnotationDefault(value)
            }
            "LocalVariableTable" => {
                let table_length = buf.read_u16()?;
                let mut entries = Vec::with_capacity((table_length as usize).min(PREALLOC_CAP));
                for _ in 0..table_length {
                    entries.push(LocalVariableEntry {
                        start_pc: buf.read_u16()?,
                        length: buf.read_u16()?,
                        name_index: buf.read_u16()?,
                        descriptor_index: buf.read_u16()?,
                        index: buf.read_u16()?,
                    });
                }
                Attribute::LocalVariableTable(entries)
            }
            "LocalVariableTypeTable" => {
                let table_length = buf.read_u16()?;
                let mut entries = Vec::with_capacity((table_length as usize).min(PREALLOC_CAP));
                for _ in 0..table_length {
                    entries.push(LocalVariableTypeEntry {
                        start_pc: buf.read_u16()?,
                        length: buf.read_u16()?,
                        name_index: buf.read_u16()?,
                        signature_index: buf.read_u16()?,
                        index: buf.read_u16()?,
                    });
                }
                Attribute::LocalVariableTypeTable(entries)
            }
            "MethodParameters" => {
                let parameters_count = buf.read_u8()?;
                let mut parameters = Vec::with_capacity((parameters_count as usize).min(PREALLOC_CAP));
                for _ in 0..parameters_count {
                    parameters.push(MethodParameter {
                        name_index: buf.read_u16()?,
                        access_flags: buf.read_u16()?,
                    });
                }
                Attribute::MethodParameters(parameters)
            }
            "LoadableDescriptors" => {
                // JEP 401 (Valhalla preview, class file 69+ with preview bit):
                //   u2 number_of_descriptors;
                //   u2 descriptors[number_of_descriptors];   // CONSTANT_Utf8_info
                let number_of_descriptors = buf.read_u16()?;
                let mut descriptors =
                    Vec::with_capacity((number_of_descriptors as usize).min(PREALLOC_CAP));
                for _ in 0..number_of_descriptors {
                    descriptors.push(buf.read_u16()?);
                }
                Attribute::LoadableDescriptors { descriptors }
            }
            _ => {
                // Unknown attribute — store raw bytes
                let data = buf.read_bytes(length)?.to_vec();
                Attribute::Unknown {
                    name: name.to_string(),
                    data,
                }
            }
        };

        attributes.push(attr);
    }

    Ok(attributes)
}

// ---------------------------------------------------------------------------
// Annotation parsing helpers (JVM spec 4.7.16–4.7.24)
// ---------------------------------------------------------------------------

/// Read a single annotation structure (4.7.16).
fn read_annotation(buf: &mut ClassFileBuffer) -> Result<Annotation, ClassReaderError> {
    let type_index = buf.read_u16()?;
    let num_element_value_pairs = buf.read_u16()?;
    let mut element_value_pairs = Vec::with_capacity((num_element_value_pairs as usize).min(PREALLOC_CAP));
    for _ in 0..num_element_value_pairs {
        let element_name_index = buf.read_u16()?;
        let value = read_element_value(buf)?;
        element_value_pairs.push(ElementValuePair {
            element_name_index,
            value,
        });
    }
    Ok(Annotation {
        type_index,
        element_value_pairs,
    })
}

/// Read an element_value structure (4.7.16.1).
///
/// Tag-dispatched recursive parsing. Tags:
/// - `B`, `C`, `D`, `F`, `I`, `J`, `S`, `Z`, `s` → const_value_index (u16)
/// - `e` → enum_const { type_name_index, const_name_index }
/// - `c` → class_info_index (u16)
/// - `@` → nested annotation
/// - `[` → array of element_values
fn read_element_value(buf: &mut ClassFileBuffer) -> Result<ElementValue, ClassReaderError> {
    let tag = buf.read_u8()?;
    match tag {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b's' => {
            let const_value_index = buf.read_u16()?;
            Ok(ElementValue::Const {
                tag,
                const_value_index,
            })
        }
        b'e' => {
            let type_name_index = buf.read_u16()?;
            let const_name_index = buf.read_u16()?;
            Ok(ElementValue::Enum {
                type_name_index,
                const_name_index,
            })
        }
        b'c' => {
            let class_info_index = buf.read_u16()?;
            Ok(ElementValue::Class { class_info_index })
        }
        b'@' => {
            let annotation = read_annotation(buf)?;
            Ok(ElementValue::AnnotationValue(annotation))
        }
        b'[' => {
            let num_values = buf.read_u16()?;
            let mut values = Vec::with_capacity((num_values as usize).min(PREALLOC_CAP));
            for _ in 0..num_values {
                values.push(read_element_value(buf)?);
            }
            Ok(ElementValue::Array(values))
        }
        _ => Err(ClassReaderError::InvalidClassData {
            message: format!("invalid element_value tag: 0x{tag:02X} ('{}')", tag as char),
        }),
    }
}

/// Read a type_annotation structure (JVM spec 4.7.20).
fn read_type_annotation(buf: &mut ClassFileBuffer) -> Result<TypeAnnotation, ClassReaderError> {
    let target_type = buf.read_u8()?;
    let target_info = read_target_info(buf, target_type)?;
    let type_path = read_type_path(buf)?;
    let annotation = read_annotation(buf)?;
    Ok(TypeAnnotation {
        target_type,
        target_info,
        type_path,
        annotation,
    })
}

/// Read target_info based on target_type (JVM spec Table 4.7.20-A/B).
///
/// There are 14 different forms. We store them as raw bytes — a pragmatic compromise
/// since full type annotation target info is not needed until verification (Phase 3).
///
/// Each match arm validates the expected byte count for its target_type:
///   0x00, 0x01              -> 1 byte  (type_parameter_target)
///   0x10                    -> 2 bytes (supertype_target)
///   0x11, 0x12              -> 2 bytes (type_parameter_bound_target)
///   0x13..=0x15             -> 0 bytes (empty_target)
///   0x16                    -> 1 byte  (formal_parameter_target)
///   0x17                    -> 2 bytes (throws_target)
///   0x40, 0x41              -> 2 + 6*N bytes (localvar_target)
///   0x42                    -> 2 bytes (catch_target)
///   0x43..=0x46             -> 2 bytes (offset_target)
///   0x47..=0x4B             -> 3 bytes (type_argument_target)
/// Unknown target_type values return an error.
fn read_target_info(
    buf: &mut ClassFileBuffer,
    target_type: u8,
) -> Result<Vec<u8>, ClassReaderError> {
    match target_type {
        // type_parameter_target: 1 byte (type_parameter_index)
        0x00 | 0x01 => {
            let b = buf.read_u8()?;
            Ok(vec![b])
        }
        // supertype_target: 2 bytes (supertype_index)
        0x10 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        // type_parameter_bound_target: 2 bytes (type_parameter_index, bound_index)
        0x11 | 0x12 => {
            let a = buf.read_u8()?;
            let b = buf.read_u8()?;
            Ok(vec![a, b])
        }
        // empty_target: 0 bytes
        0x13..=0x15 => Ok(vec![]),
        // formal_parameter_target: 1 byte (formal_parameter_index)
        0x16 => {
            let b = buf.read_u8()?;
            Ok(vec![b])
        }
        // throws_target: 2 bytes (throws_type_index)
        0x17 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        // localvar_target: variable length (table_length + entries)
        0x40 | 0x41 => {
            let table_length = buf.read_u16()?;
            let byte_count = 6 * table_length as usize; // 3 × u16 per entry
            let mut data = Vec::with_capacity(2 + byte_count);
            data.push((table_length >> 8) as u8);
            data.push(table_length as u8);
            let raw = buf.read_bytes(byte_count)?;
            data.extend_from_slice(raw);
            Ok(data)
        }
        // catch_target: 2 bytes (exception_table_index)
        0x42 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        // offset_target: 2 bytes (offset)
        0x43..=0x46 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        // type_argument_target: 3 bytes (offset u16 + type_argument_index u8)
        0x47..=0x4B => {
            let a = buf.read_u8()?;
            let b = buf.read_u8()?;
            let c = buf.read_u8()?;
            Ok(vec![a, b, c])
        }
        _ => Err(ClassReaderError::InvalidClassData {
            message: format!("unknown type annotation target_type: 0x{target_type:02X}"),
        }),
    }
}

/// Read a type_path structure (JVM spec 4.7.20.2).
fn read_type_path(buf: &mut ClassFileBuffer) -> Result<Vec<TypePathEntry>, ClassReaderError> {
    let path_length = buf.read_u8()?;
    let mut path = Vec::with_capacity((path_length as usize).min(PREALLOC_CAP));
    for _ in 0..path_length {
        path.push(TypePathEntry {
            type_path_kind: buf.read_u8()?,
            type_argument_index: buf.read_u8()?,
        });
    }
    Ok(path)
}

fn read_code_attribute(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
) -> Result<Attribute, ClassReaderError> {
    let max_stack = buf.read_u16()?;
    let max_locals = buf.read_u16()?;

    let code_length = buf.read_u32()? as usize;
    // JVM spec 4.7.3: code_length must be > 0 and <= 65535
    const MAX_CODE_LENGTH: usize = 65535;
    if code_length == 0 || code_length > MAX_CODE_LENGTH {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "Code attribute code_length {code_length} outside valid range 1..={MAX_CODE_LENGTH}"
            ),
        });
    }
    let code = buf.read_bytes(code_length)?.to_vec();

    let exception_table_length = buf.read_u16()?;
    validate_count("exception_table", exception_table_length, MAX_EXCEPTION_TABLE_COUNT)?;
    let mut exception_table = Vec::with_capacity((exception_table_length as usize).min(PREALLOC_CAP));
    for _ in 0..exception_table_length {
        exception_table.push(ExceptionTableEntry {
            start_pc: buf.read_u16()?,
            end_pc: buf.read_u16()?,
            handler_pc: buf.read_u16()?,
            catch_type: buf.read_u16()?,
        });
    }

    let attributes = read_attributes(buf, constant_pool)?;

    Ok(Attribute::Code(CodeAttribute {
        max_stack,
        max_locals,
        code,
        exception_table,
        attributes,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: write a u16 big-endian to a vec.
    fn push_u16(buf: &mut Vec<u8>, val: u16) {
        buf.push((val >> 8) as u8);
        buf.push(val as u8);
    }

    // ── Annotation parsing tests ──────────────────────────────────────────

    #[test]
    fn read_annotation_simple_string_element() {
        // Annotation: type_index=5, 1 element pair (name_index=10, tag='s', const_value_index=20)
        let mut data = Vec::new();
        push_u16(&mut data, 5); // type_index
        push_u16(&mut data, 1); // num_element_value_pairs
        push_u16(&mut data, 10); // element_name_index
        data.push(b's'); // tag
        push_u16(&mut data, 20); // const_value_index

        let mut buf = ClassFileBuffer::new(&data);
        let ann = read_annotation(&mut buf).unwrap();
        assert_eq!(ann.type_index, 5);
        assert_eq!(ann.element_value_pairs.len(), 1);
        assert_eq!(ann.element_value_pairs[0].element_name_index, 10);
        match &ann.element_value_pairs[0].value {
            ElementValue::Const {
                tag,
                const_value_index,
            } => {
                assert_eq!(*tag, b's');
                assert_eq!(*const_value_index, 20);
            }
            other => panic!("Expected Const, got {other:?}"),
        }
    }

    #[test]
    fn read_element_value_enum_constant() {
        // tag='e', type_name_index=3, const_name_index=7
        let data = [b'e', 0x00, 0x03, 0x00, 0x07];
        let mut buf = ClassFileBuffer::new(&data);
        let ev = read_element_value(&mut buf).unwrap();
        match ev {
            ElementValue::Enum {
                type_name_index,
                const_name_index,
            } => {
                assert_eq!(type_name_index, 3);
                assert_eq!(const_name_index, 7);
            }
            other => panic!("Expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn read_element_value_class_literal() {
        // tag='c', class_info_index=42
        let data = [b'c', 0x00, 0x2A];
        let mut buf = ClassFileBuffer::new(&data);
        let ev = read_element_value(&mut buf).unwrap();
        match ev {
            ElementValue::Class { class_info_index } => {
                assert_eq!(class_info_index, 42);
            }
            other => panic!("Expected Class, got {other:?}"),
        }
    }

    #[test]
    fn read_element_value_nested_annotation() {
        // tag='@', annotation { type_index=8, 0 pairs }
        let data = [b'@', 0x00, 0x08, 0x00, 0x00];
        let mut buf = ClassFileBuffer::new(&data);
        let ev = read_element_value(&mut buf).unwrap();
        match ev {
            ElementValue::AnnotationValue(ann) => {
                assert_eq!(ann.type_index, 8);
                assert!(ann.element_value_pairs.is_empty());
            }
            other => panic!("Expected AnnotationValue, got {other:?}"),
        }
    }

    #[test]
    fn read_element_value_array() {
        // tag='[', 2 elements: (tag='I', index=1), (tag='I', index=2)
        let mut data = Vec::new();
        data.push(b'[');
        push_u16(&mut data, 2); // num_values
        data.push(b'I');
        push_u16(&mut data, 1);
        data.push(b'I');
        push_u16(&mut data, 2);

        let mut buf = ClassFileBuffer::new(&data);
        let ev = read_element_value(&mut buf).unwrap();
        match ev {
            ElementValue::Array(values) => {
                assert_eq!(values.len(), 2);
                match &values[0] {
                    ElementValue::Const {
                        tag,
                        const_value_index,
                    } => {
                        assert_eq!(*tag, b'I');
                        assert_eq!(*const_value_index, 1);
                    }
                    other => panic!("Expected Const, got {other:?}"),
                }
            }
            other => panic!("Expected Array, got {other:?}"),
        }
    }

    #[test]
    fn read_element_value_invalid_tag() {
        let data = [b'X', 0x00, 0x01];
        let mut buf = ClassFileBuffer::new(&data);
        assert!(read_element_value(&mut buf).is_err());
    }

    #[test]
    fn read_annotation_default_const() {
        // AnnotationDefault with const int value
        let mut data = Vec::new();
        data.push(b'I'); // tag
        push_u16(&mut data, 42); // const_value_index

        let mut buf = ClassFileBuffer::new(&data);
        let ev = read_element_value(&mut buf).unwrap();
        match ev {
            ElementValue::Const {
                tag,
                const_value_index,
            } => {
                assert_eq!(tag, b'I');
                assert_eq!(const_value_index, 42);
            }
            other => panic!("Expected Const, got {other:?}"),
        }
    }

    // ── Type annotation tests ────────────────────────────────────────────

    #[test]
    fn read_type_annotation_empty_target() {
        // target_type=0x13 (empty_target), empty type_path, annotation { type=5, 0 pairs }
        let mut data = Vec::new();
        data.push(0x13); // target_type (METHOD_RETURN = empty)
        data.push(0x00); // type_path length = 0
        push_u16(&mut data, 5); // annotation type_index
        push_u16(&mut data, 0); // 0 element value pairs

        let mut buf = ClassFileBuffer::new(&data);
        let ta = read_type_annotation(&mut buf).unwrap();
        assert_eq!(ta.target_type, 0x13);
        assert!(ta.target_info.is_empty());
        assert!(ta.type_path.is_empty());
        assert_eq!(ta.annotation.type_index, 5);
    }

    #[test]
    fn read_type_annotation_with_type_path() {
        // target_type=0x00 (type_parameter_target, 1 byte), type_path with 1 entry
        let mut data = vec![
            0x00, // target_type
            0x02, // type_parameter_index = 2
            0x01, // type_path length = 1
            0x03, // type_path_kind = 3 (type argument)
            0x01, // type_argument_index = 1
        ];
        push_u16(&mut data, 7); // annotation type_index
        push_u16(&mut data, 0); // 0 element value pairs

        let mut buf = ClassFileBuffer::new(&data);
        let ta = read_type_annotation(&mut buf).unwrap();
        assert_eq!(ta.target_type, 0x00);
        assert_eq!(ta.target_info, vec![0x02]);
        assert_eq!(ta.type_path.len(), 1);
        assert_eq!(ta.type_path[0].type_path_kind, 3);
        assert_eq!(ta.type_path[0].type_argument_index, 1);
    }

    #[test]
    fn read_target_info_type_argument_target() {
        // target_type=0x47 → 3 bytes (offset u16 + type_argument_index u8)
        let data = [0x00, 0x0A, 0x01]; // offset=10, type_argument_index=1
        let mut buf = ClassFileBuffer::new(&data);
        let info = read_target_info(&mut buf, 0x47).unwrap();
        assert_eq!(info, vec![0x00, 0x0A, 0x01]);
    }

    #[test]
    fn read_target_info_unknown_target_type() {
        let data = [0xFF];
        let mut buf = ClassFileBuffer::new(&data);
        assert!(read_target_info(&mut buf, 0xFF).is_err());
    }

    // ── Module attribute parsing tests ───────────────────────────────────

    /// Helper to build a minimal constant pool and parse a single attribute.
    fn parse_single_attribute(attr_name: &str, attr_data: &[u8]) -> Attribute {
        // Build a CP with: [0]=Tombstone, [1]=Utf8(attr_name)
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(attr_name.into()),
        ]);

        // Construct the raw attribute bytes:
        // u16 attribute_name_index (1), u32 attribute_length, then data
        let mut raw = Vec::new();
        push_u16(&mut raw, 1); // name index
        let len = attr_data.len() as u32;
        raw.push((len >> 24) as u8);
        raw.push((len >> 16) as u8);
        raw.push((len >> 8) as u8);
        raw.push(len as u8);
        raw.extend_from_slice(attr_data);

        // Prepend u16 attributes_count = 1
        let mut full = Vec::new();
        push_u16(&mut full, 1);
        full.extend_from_slice(&raw);

        let mut buf = ClassFileBuffer::new(&full);
        let attrs = read_attributes(&mut buf, &cp).unwrap();
        assert_eq!(attrs.len(), 1);
        attrs.into_iter().next().unwrap()
    }

    #[test]
    fn parse_module_main_class() {
        let mut data = Vec::new();
        push_u16(&mut data, 42); // main_class_index
        let attr = parse_single_attribute("ModuleMainClass", &data);
        match attr {
            Attribute::ModuleMainClass { main_class_index } => {
                assert_eq!(main_class_index, 42);
            }
            other => panic!("Expected ModuleMainClass, got {other:?}"),
        }
    }

    #[test]
    fn parse_module_packages() {
        let mut data = Vec::new();
        push_u16(&mut data, 3); // count
        push_u16(&mut data, 10);
        push_u16(&mut data, 20);
        push_u16(&mut data, 30);
        let attr = parse_single_attribute("ModulePackages", &data);
        match attr {
            Attribute::ModulePackages { packages } => {
                assert_eq!(packages, vec![10, 20, 30]);
            }
            other => panic!("Expected ModulePackages, got {other:?}"),
        }
    }

    #[test]
    fn parse_module_empty() {
        // Module with name_index=5, flags=0, version_index=0, no requires/exports/opens/uses/provides
        let mut data = Vec::new();
        push_u16(&mut data, 5); // name_index
        push_u16(&mut data, 0); // flags
        push_u16(&mut data, 0); // version_index
        push_u16(&mut data, 0); // requires_count
        push_u16(&mut data, 0); // exports_count
        push_u16(&mut data, 0); // opens_count
        push_u16(&mut data, 0); // uses_count
        push_u16(&mut data, 0); // provides_count
        let attr = parse_single_attribute("Module", &data);
        match attr {
            Attribute::Module {
                name_index,
                flags,
                version_index,
                requires,
                exports,
                opens,
                uses,
                provides,
            } => {
                assert_eq!(name_index, 5);
                assert_eq!(flags, 0);
                assert_eq!(version_index, 0);
                assert!(requires.is_empty());
                assert!(exports.is_empty());
                assert!(opens.is_empty());
                assert!(uses.is_empty());
                assert!(provides.is_empty());
            }
            other => panic!("Expected Module, got {other:?}"),
        }
    }

    #[test]
    fn parse_module_with_requires_and_exports() {
        let mut data = Vec::new();
        push_u16(&mut data, 5); // name_index
        push_u16(&mut data, 0); // flags
        push_u16(&mut data, 0); // version_index

        // 1 requires entry
        push_u16(&mut data, 1); // requires_count
        push_u16(&mut data, 10); // requires_index
        push_u16(&mut data, 0x0020); // requires_flags (ACC_MANDATED)
        push_u16(&mut data, 11); // requires_version_index

        // 1 exports entry with 2 exports_to
        push_u16(&mut data, 1); // exports_count
        push_u16(&mut data, 20); // exports_index
        push_u16(&mut data, 0); // exports_flags
        push_u16(&mut data, 2); // exports_to_count
        push_u16(&mut data, 30);
        push_u16(&mut data, 31);

        push_u16(&mut data, 0); // opens_count
        push_u16(&mut data, 0); // uses_count
        push_u16(&mut data, 0); // provides_count

        let attr = parse_single_attribute("Module", &data);
        match attr {
            Attribute::Module {
                requires, exports, ..
            } => {
                assert_eq!(requires.len(), 1);
                assert_eq!(requires[0].requires_index, 10);
                assert_eq!(requires[0].requires_flags, 0x0020);
                assert_eq!(requires[0].requires_version_index, 11);

                assert_eq!(exports.len(), 1);
                assert_eq!(exports[0].exports_index, 20);
                assert_eq!(exports[0].exports_to, vec![30, 31]);
            }
            other => panic!("Expected Module, got {other:?}"),
        }
    }

    // ── Resource limits / safety tests ───────────────────────────────────

    #[test]
    fn prealloc_cap_constant_exists_and_is_bounded() {
        // Verify the PREALLOC_CAP constant is set to a reasonable value
        // to prevent excessive pre-allocation on malformed class files.
        assert!(PREALLOC_CAP > 0, "PREALLOC_CAP must be positive");
        assert!(
            PREALLOC_CAP <= 65536,
            "PREALLOC_CAP should be bounded to prevent excessive allocation"
        );
    }

    #[test]
    fn truncated_class_file_returns_error() {
        // A truncated file (just the magic number) should fail gracefully
        let data = [0xCA, 0xFE, 0xBA, 0xBE];
        let result = read_class(&data);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_magic_number_returns_error() {
        let data = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x34];
        let result = read_class(&data);
        assert!(result.is_err());
    }

    #[test]
    fn empty_input_returns_error() {
        let result = read_class(&[]);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------
    // T10: StringPool consumer wiring — verify the reader's constant-pool
    // hot path produces deduplicated `Arc<str>` storage via the global
    // string pool.
    // ---------------------------------------------------------------------

    #[test]
    fn t10_intern_reader_constant_pool_deduplicates() {
        // Parse a real class file. Each distinct UTF-8 string in its
        // constant pool must share its `Arc<str>` allocation with every
        // other pool entry holding the same content — and with every
        // matching string returned by `rustjvm_types::intern_arc`.
        let class_bytes = include_bytes!("../../test_classes/HelloWorld.class");
        let class_file = read_class(class_bytes).expect("HelloWorld.class must parse");

        // Collect all Utf8 entries so we can inspect their Arc<str> pointers.
        let mut utf8_arcs: Vec<std::sync::Arc<str>> = Vec::new();
        for idx in 1..class_file.constant_pool.len() {
            if let Some(arc) = class_file.constant_pool.get_utf8_arc(idx as u16) {
                utf8_arcs.push(arc);
            }
        }
        assert!(utf8_arcs.len() >= 3, "HelloWorld has multiple Utf8 entries");

        // For every pair of Utf8 entries whose bytes are equal, the Arc<str>
        // must share a single backing allocation (Arc::ptr_eq).
        for (i, a) in utf8_arcs.iter().enumerate() {
            for (j, b) in utf8_arcs.iter().enumerate() {
                if i != j && **a == **b {
                    assert!(
                        std::sync::Arc::ptr_eq(a, b),
                        "duplicate Utf8 content '{}' must share Arc<str> allocation",
                        a
                    );
                }
            }
        }

        // And every Utf8 in the pool must match the global pool's interned
        // Arc<str> for the same content — proof the reader funnels through
        // `rustjvm_types::intern_arc` at parse time.
        for arc in &utf8_arcs {
            let pooled = rustjvm_types::intern_arc(arc);
            assert!(
                std::sync::Arc::ptr_eq(arc, &pooled),
                "pool-stored Utf8 '{}' must match global pool intern",
                arc
            );
        }
    }

    #[test]
    fn t10_intern_reader_cp_same_bytes_shared_allocation() {
        // Synthesize a minimal class file whose constant pool has
        // five Utf8 entries for the same string — after parsing, all
        // five must point to the same Arc<str> allocation.
        //
        // Class file layout (JVMS §4.1 minimal):
        //   magic(4) minor(2) major(2) cp_count(2) cp_entries(...)
        //   access(2) this(2) super(2) ifaces(2=0)
        //   fields(2=0) methods(2=0) attrs(2=0)
        let mut data = Vec::<u8>::new();
        // magic
        data.extend_from_slice(&0xCAFEBABE_u32.to_be_bytes());
        // minor 0, major 52 (Java 8)
        data.extend_from_slice(&0_u16.to_be_bytes());
        data.extend_from_slice(&52_u16.to_be_bytes());
        // constant_pool_count = 8 (indices 1..=7)
        //   1..=5 = Utf8 "java/lang/Object"
        //   6     = Class { name_index = 1 }
        //   7     = Utf8 "Test"
        data.extend_from_slice(&8_u16.to_be_bytes());
        for _ in 0..5 {
            data.push(1); // tag CONSTANT_Utf8
            let s = b"java/lang/Object";
            data.extend_from_slice(&(s.len() as u16).to_be_bytes());
            data.extend_from_slice(s);
        }
        // #6 Class -> name_index 1
        data.push(7);
        data.extend_from_slice(&1_u16.to_be_bytes());
        // #7 Utf8 "Test"
        data.push(1);
        data.extend_from_slice(&4_u16.to_be_bytes());
        data.extend_from_slice(b"Test");
        // access ACC_PUBLIC | ACC_SUPER
        data.extend_from_slice(&0x0021_u16.to_be_bytes());
        // this_class index = 6, super_class index = 0 (no super)
        data.extend_from_slice(&6_u16.to_be_bytes());
        data.extend_from_slice(&0_u16.to_be_bytes());
        // interfaces_count=0, fields_count=0, methods_count=0, attributes_count=0
        data.extend_from_slice(&0_u16.to_be_bytes());
        data.extend_from_slice(&0_u16.to_be_bytes());
        data.extend_from_slice(&0_u16.to_be_bytes());
        data.extend_from_slice(&0_u16.to_be_bytes());

        let cf = read_class(&data).expect("synthetic class file must parse");
        let arcs: Vec<std::sync::Arc<str>> = (1..=5u16)
            .map(|i| {
                cf.constant_pool
                    .get_utf8_arc(i)
                    .expect("Utf8 entry expected")
            })
            .collect();

        // All five Arcs must share a single backing allocation.
        for a in &arcs {
            assert_eq!(&**a, "java/lang/Object");
            assert!(
                std::sync::Arc::ptr_eq(a, &arcs[0]),
                "all five Utf8 entries must share the same Arc<str>"
            );
        }
    }
}
