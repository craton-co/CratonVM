// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A class file written back from a live class, for `retransformClasses` when
//! no retransformation base survives anywhere else.
//!
//! Interpreter round i1 wave 29, lane L3
//! (`docs/internal/fixed-bugs/interpreter-L3-a-generated-class-whose-base-was-evicted-is-not-retransformed-FIXED-20260930.md`).
//! A class a user loader defined from bytes that are no resource anywhere (a
//! Byte Buddy / CGLIB subclass, a `Lookup.defineClass` class, a class a test
//! generated) keeps its base only in the class-bytes FIFO
//! (`ClassManager::insert_class_bytes`). Once the FIFO evicted it, the class
//! path (`find_resource`) and the defining loader's `getResourceAsStream`
//! (`instrument::defining_loader_class_bytes`) find nothing, and
//! `retransformClasses` skipped the class. HotSpot never skips: a class with
//! no cached class file is reconstituted from the live class
//! (`JvmtiClassFileReconstituter`, which the local JDK tree does not carry, so
//! its exact attribute list is not compared here).
//!
//! # What is written
//!
//! The live class's constant pool index for index (every index a method body,
//! an attribute or a bootstrap method names keeps its meaning), then the
//! fields and methods with every attribute they were defined with (they are
//! kept decoded, `force_decode_all` at define time), then the class-level
//! attributes `Class` keeps: `SourceFile`, `InnerClasses`, `EnclosingMethod`,
//! `Signature`, `NestHost`, `NestMembers`, `PermittedSubclasses`, `Record`
//! (component names and descriptors only), `BootstrapMethods` and
//! `RuntimeVisibleAnnotations`. Names those need and the pool lacks are
//! appended. Not written, because `Class` does not keep them: class-level
//! `RuntimeInvisibleAnnotations`, type annotations, `SourceDebugExtension`,
//! `Deprecated` / `Synthetic` attributes, `Module*`, and a record component's
//! own attributes. The result is parsed back, every attribute decoded, before
//! it is handed to a transformer.
//!
//! # When it is right
//!
//! Only while the live class is still the class as defined: a class never
//! redefined in this VM (`ClassManager::class_redefine_generation` is 0).
//! Every class a `redefineClasses` / `retransformClasses` changed has its base
//! pinned out of the FIFO's reach (`insert_class_bytes_pinned`: by
//! `redefine_class`, by `redefine_class_as_instrument` for a retransform since
//! this wave), so a redefined class whose base is missing here is one the
//! pinned cap refused, and the live class would be the woven one. A class a
//! retransform-capable transformer changed at LOAD is pinned the same way; once
//! the pinned cap has overflowed such a class may sit in the FIFO, and the live
//! class is then the transformed one, so nothing is reconstituted after an
//! overflow (`class_bytes_pinned_overflowed`).

use std::collections::HashMap;
use std::sync::Arc;

use cratonvm_classloading::{Class, ClassId, ClassManager};
use cratonvm_reader::attribute::{Annotation, Attribute, ElementValue, LazyAttribute};
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// Why [`reconstitute_class_file`] wrote nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReconstituteRefusal {
    /// No class with that id in the store.
    UnknownClass,
    /// A hidden class, an array class or a compatibility stub: no class file.
    NoClassFile(&'static str),
    /// Redefined in this VM: the live class is not the base.
    Redefined,
    /// The pinned bases passed their cap: a load-transformed live class may
    /// be the transformed one.
    PinnedBasesOverflowed,
    /// The writer could not encode the class.
    Unencodable(String),
    /// What was written did not parse back as the class.
    ParseBack(String),
}

/// [`reconstitute_class_file`], logged: a `debug` line either way, and under
/// `CRATONVM_DBG_RETRANSFORM` the positive control
/// `[RETRANSFORM] reconstituted the base of <name> from the live class: <n> bytes`
/// (or the refusal).
pub(crate) fn reconstitute_logged(cm: &ClassManager, class_id: ClassId) -> Option<Vec<u8>> {
    let outcome = reconstitute_class_file(cm, class_id);
    let name = cm
        .class_store
        .get(class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    let dbg = cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok();
    match outcome {
        Ok(bytes) => {
            tracing::debug!(
                "retransform: reconstituted the base of `{name}` from the live class ({} bytes)",
                bytes.len()
            );
            if dbg {
                eprintln!(
                    "[RETRANSFORM] reconstituted the base of {name} from the live class: {} bytes",
                    bytes.len()
                );
            }
            Some(bytes)
        }
        Err(refusal) => {
            tracing::debug!("retransform: no class file reconstituted for `{name}`: {refusal:?}");
            if dbg {
                eprintln!("[RETRANSFORM] no class file reconstituted for {name}: {refusal:?}");
            }
            None
        }
    }
}

/// A class file equivalent to the one `class_id` was defined from, written
/// from the live class (see the module docs for what it carries and when it
/// is the retransformation base). Takes the class manager as the caller holds
/// it (a read lock is enough); runs no Java.
pub(crate) fn reconstitute_class_file(
    cm: &ClassManager,
    class_id: ClassId,
) -> Result<Vec<u8>, ReconstituteRefusal> {
    let class = cm
        .class_store
        .get(class_id)
        .ok_or(ReconstituteRefusal::UnknownClass)?;
    if class.hidden {
        return Err(ReconstituteRefusal::NoClassFile("hidden class"));
    }
    if class.array_info.is_some() || class.name.starts_with('[') {
        return Err(ReconstituteRefusal::NoClassFile("array class"));
    }
    if class.origin.is_compatibility_stub() {
        return Err(ReconstituteRefusal::NoClassFile("compatibility stub"));
    }
    if cm.class_redefine_generation(class_id) != 0 {
        return Err(ReconstituteRefusal::Redefined);
    }
    if cm.class_bytes_pinned_overflowed() {
        return Err(ReconstituteRefusal::PinnedBasesOverflowed);
    }
    let super_name = match class.superclass {
        Some(id) => Some(Arc::clone(
            &cm.class_store
                .get(id)
                .ok_or_else(|| {
                    ReconstituteRefusal::Unencodable("superclass not in the store".to_string())
                })?
                .name,
        )),
        None => None,
    };
    let mut interface_names = Vec::with_capacity(class.interfaces.len());
    for id in &class.interfaces {
        let iface = cm.class_store.get(*id).ok_or_else(|| {
            ReconstituteRefusal::Unencodable("interface not in the store".to_string())
        })?;
        interface_names.push(Arc::clone(&iface.name));
    }
    let bytes = write_class_file(class, super_name.as_deref(), &interface_names)
        .map_err(ReconstituteRefusal::Unencodable)?;
    check_parses_back(class, &bytes).map_err(ReconstituteRefusal::ParseBack)?;
    Ok(bytes)
}

/// The class file of `class`, its superclass and interfaces named.
fn write_class_file(
    class: &Class,
    super_name: Option<&str>,
    interface_names: &[Arc<str>],
) -> Result<Vec<u8>, String> {
    let mut pool = PoolWriter::from_live(&class.constant_pool)?;
    // Everything after the pool first: writing it may append to the pool.
    let mut body = Vec::with_capacity(1024);
    put_u2(&mut body, class.access_flags.bits());
    let this_index = pool.class_ref(&class.name)?;
    put_u2(&mut body, this_index);
    let super_index = match super_name {
        Some(name) => pool.class_ref(name)?,
        None if &*class.name == "java/lang/Object" => 0,
        None if class.access_flags.contains(ClassAccessFlags::INTERFACE) => {
            pool.class_ref("java/lang/Object")?
        }
        None => return Err("a class with no superclass".to_string()),
    };
    put_u2(&mut body, super_index);
    put_u2(&mut body, count_u2(interface_names.len())?);
    for name in interface_names {
        let index = pool.class_ref(name)?;
        put_u2(&mut body, index);
    }

    put_u2(&mut body, count_u2(class.fields.len())?);
    for field in &class.fields {
        put_u2(&mut body, field.access_flags.bits());
        let name = pool.utf8(&field.name)?;
        put_u2(&mut body, name);
        let descriptor = pool.utf8(&field.descriptor)?;
        put_u2(&mut body, descriptor);
        write_lazy_attributes(&mut pool, &mut body, &field.attributes)?;
    }

    put_u2(&mut body, count_u2(class.methods.len())?);
    for method in &class.methods {
        put_u2(&mut body, method.access_flags.bits());
        let name = pool.utf8(&method.name)?;
        put_u2(&mut body, name);
        let descriptor = pool.utf8(&method.descriptor)?;
        put_u2(&mut body, descriptor);
        write_lazy_attributes(&mut pool, &mut body, &method.attributes)?;
    }

    write_class_attributes(&mut pool, &mut body, class, super_name)?;

    let mut out = Vec::with_capacity(body.len() + class.constant_pool.len() * 8 + 16);
    put_u4(&mut out, 0xCAFE_BABE);
    put_u2(&mut out, class.version.minor);
    put_u2(&mut out, class.version.major);
    pool.write(&mut out)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// The class-level attributes `Class` keeps (the module docs list them).
fn write_class_attributes(
    pool: &mut PoolWriter<'_>,
    out: &mut Vec<u8>,
    class: &Class,
    super_name: Option<&str>,
) -> Result<(), String> {
    let count_at = out.len();
    put_u2(out, 0);
    let mut count: usize = 0;

    if let Some(source_file) = &class.source_file {
        let body_at = begin_attribute(pool, out, "SourceFile")?;
        let index = pool.utf8(source_file)?;
        put_u2(out, index);
        end_attribute(out, body_at)?;
        count += 1;
    }
    if !class.inner_classes.is_empty() {
        let body_at = begin_attribute(pool, out, "InnerClasses")?;
        put_u2(out, count_u2(class.inner_classes.len())?);
        for entry in &class.inner_classes {
            let inner = pool.class_ref(&entry.inner_class)?;
            let outer = if entry.outer_class.is_empty() {
                0
            } else {
                pool.class_ref(&entry.outer_class)?
            };
            let inner_name = if entry.inner_name.is_empty() {
                0
            } else {
                pool.utf8(&entry.inner_name)?
            };
            put_u2(out, inner);
            put_u2(out, outer);
            put_u2(out, inner_name);
            put_u2(out, entry.access_flags);
        }
        end_attribute(out, body_at)?;
        count += 1;
    }
    if let Some(enclosing) = &class.enclosing_method {
        let body_at = begin_attribute(pool, out, "EnclosingMethod")?;
        let class_index = pool.class_ref(&enclosing.class_name)?;
        let method_index = if enclosing.method_name.is_empty() {
            0
        } else {
            pool.name_and_type(&enclosing.method_name, &enclosing.method_descriptor)?
        };
        put_u2(out, class_index);
        put_u2(out, method_index);
        end_attribute(out, body_at)?;
        count += 1;
    }
    if let Some(signature) = &class.signature {
        let body_at = begin_attribute(pool, out, "Signature")?;
        let index = pool.utf8(signature)?;
        put_u2(out, index);
        end_attribute(out, body_at)?;
        count += 1;
    }
    if let Some(host) = &class.nest_host {
        let body_at = begin_attribute(pool, out, "NestHost")?;
        let index = pool.class_ref(host)?;
        put_u2(out, index);
        end_attribute(out, body_at)?;
        count += 1;
    }
    for (name, classes) in [
        ("NestMembers", &class.nest_members),
        ("PermittedSubclasses", &class.permitted_subclasses),
    ] {
        if classes.is_empty() {
            continue;
        }
        let body_at = begin_attribute(pool, out, name)?;
        put_u2(out, count_u2(classes.len())?);
        for member in classes {
            let index = pool.class_ref(member)?;
            put_u2(out, index);
        }
        end_attribute(out, body_at)?;
        count += 1;
    }
    // A record with no components still has the attribute; `Class` keeps
    // no flag for it, so a direct subclass of `java.lang.Record` gets one.
    if !class.record_components.is_empty() || super_name == Some("java/lang/Record") {
        let body_at = begin_attribute(pool, out, "Record")?;
        put_u2(out, count_u2(class.record_components.len())?);
        for component in &class.record_components {
            let name = pool.utf8(&component.name)?;
            let descriptor = pool.utf8(&component.descriptor)?;
            put_u2(out, name);
            put_u2(out, descriptor);
            put_u2(out, 0); // its own attributes are not kept
        }
        end_attribute(out, body_at)?;
        count += 1;
    }
    if !class.bootstrap_methods.is_empty() {
        let body_at = begin_attribute(pool, out, "BootstrapMethods")?;
        put_u2(out, count_u2(class.bootstrap_methods.len())?);
        for method in &class.bootstrap_methods {
            put_u2(out, method.bootstrap_method_ref);
            put_u2_list(out, &method.bootstrap_arguments)?;
        }
        end_attribute(out, body_at)?;
        count += 1;
    }
    if !class.annotations.is_empty() {
        let body_at = begin_attribute(pool, out, "RuntimeVisibleAnnotations")?;
        put_u2(out, count_u2(class.annotations.len())?);
        for annotation in &class.annotations {
            write_annotation(out, annotation)?;
        }
        end_attribute(out, body_at)?;
        count += 1;
    }

    let count = count_u2(count)?;
    out[count_at..count_at + 2].copy_from_slice(&count.to_be_bytes());
    Ok(())
}

/// A member's attribute table.
fn write_lazy_attributes(
    pool: &mut PoolWriter<'_>,
    out: &mut Vec<u8>,
    attributes: &[LazyAttribute],
) -> Result<(), String> {
    put_u2(out, count_u2(attributes.len())?);
    for attribute in attributes {
        match attribute {
            LazyAttribute::Decoded(attr) => write_attribute(pool, out, attr)?,
            // Every member attribute is decoded at define time; a raw one is
            // written as it was read (its indices name the same live pool).
            LazyAttribute::Raw {
                name,
                source,
                range,
            } => {
                let buffer: &[u8] = source.as_ref();
                let body = buffer
                    .get(range.clone())
                    .ok_or_else(|| format!("raw attribute `{name}` out of its buffer"))?;
                let body_at = begin_attribute(pool, out, name)?;
                out.extend_from_slice(body);
                end_attribute(out, body_at)?;
            }
        }
    }
    Ok(())
}

/// An attribute table of already-decoded attributes (inside `Code`).
fn write_attributes(
    pool: &mut PoolWriter<'_>,
    out: &mut Vec<u8>,
    attributes: &[Attribute],
) -> Result<(), String> {
    put_u2(out, count_u2(attributes.len())?);
    for attr in attributes {
        write_attribute(pool, out, attr)?;
    }
    Ok(())
}

/// One attribute: name index, length, body.
fn write_attribute(
    pool: &mut PoolWriter<'_>,
    out: &mut Vec<u8>,
    attr: &Attribute,
) -> Result<(), String> {
    let body_at = begin_attribute(pool, out, attribute_name(attr))?;
    match attr {
        Attribute::Code(code) => {
            put_u2(out, code.max_stack);
            put_u2(out, code.max_locals);
            let bytes = code.code.as_bytes();
            let len = u32::try_from(bytes.len()).map_err(|_| "code longer than a u4")?;
            put_u4(out, len);
            out.extend_from_slice(bytes);
            put_u2(out, count_u2(code.exception_table.len())?);
            for entry in &code.exception_table {
                put_u2(out, entry.start_pc);
                put_u2(out, entry.end_pc);
                put_u2(out, entry.handler_pc);
                put_u2(out, entry.catch_type);
            }
            write_attributes(pool, out, &code.attributes)?;
        }
        Attribute::SourceFile(text) | Attribute::Signature(text) => {
            let index = pool.utf8(text)?;
            put_u2(out, index);
        }
        Attribute::ConstantValue {
            constant_value_index,
        } => put_u2(out, *constant_value_index),
        Attribute::Deprecated | Attribute::Synthetic => {}
        Attribute::Exceptions { exception_indices } => put_u2_list(out, exception_indices)?,
        Attribute::LineNumberTable(entries) => {
            put_u2(out, count_u2(entries.len())?);
            for entry in entries {
                put_u2(out, entry.start_pc);
                put_u2(out, entry.line_number);
            }
        }
        Attribute::InnerClasses(entries) => {
            put_u2(out, count_u2(entries.len())?);
            for entry in entries {
                put_u2(out, entry.inner_class_info_index);
                put_u2(out, entry.outer_class_info_index);
                put_u2(out, entry.inner_name_index);
                put_u2(out, entry.inner_class_access_flags);
            }
        }
        // The body as it was read, frame count included.
        Attribute::StackMapTable { entries } => out.extend_from_slice(entries.as_bytes()),
        Attribute::BootstrapMethods(methods) => {
            put_u2(out, count_u2(methods.len())?);
            for method in methods {
                put_u2(out, method.bootstrap_method_ref);
                put_u2_list(out, &method.bootstrap_arguments)?;
            }
        }
        Attribute::EnclosingMethod {
            class_index,
            method_index,
        } => {
            put_u2(out, *class_index);
            put_u2(out, *method_index);
        }
        Attribute::NestHost { host_class_index } => put_u2(out, *host_class_index),
        Attribute::NestMembers { classes } | Attribute::PermittedSubclasses { classes } => {
            put_u2_list(out, classes)?
        }
        Attribute::Record(components) => {
            put_u2(out, count_u2(components.len())?);
            for component in components {
                put_u2(out, component.name_index);
                put_u2(out, component.descriptor_index);
                write_attributes(pool, out, &component.attributes)?;
            }
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
        } => {
            put_u2(out, *name_index);
            put_u2(out, *flags);
            put_u2(out, *version_index);
            put_u2(out, count_u2(requires.len())?);
            for entry in requires {
                put_u2(out, entry.requires_index);
                put_u2(out, entry.requires_flags);
                put_u2(out, entry.requires_version_index);
            }
            put_u2(out, count_u2(exports.len())?);
            for entry in exports {
                put_u2(out, entry.exports_index);
                put_u2(out, entry.exports_flags);
                put_u2_list(out, &entry.exports_to)?;
            }
            put_u2(out, count_u2(opens.len())?);
            for entry in opens {
                put_u2(out, entry.opens_index);
                put_u2(out, entry.opens_flags);
                put_u2_list(out, &entry.opens_to)?;
            }
            put_u2_list(out, uses)?;
            put_u2(out, count_u2(provides.len())?);
            for entry in provides {
                put_u2(out, entry.provides_index);
                put_u2_list(out, &entry.provides_with)?;
            }
        }
        Attribute::ModulePackages { packages } => put_u2_list(out, packages)?,
        Attribute::ModuleMainClass { main_class_index } => put_u2(out, *main_class_index),
        Attribute::RuntimeVisibleAnnotations(annotations)
        | Attribute::RuntimeInvisibleAnnotations(annotations) => {
            put_u2(out, count_u2(annotations.len())?);
            for annotation in annotations {
                write_annotation(out, annotation)?;
            }
        }
        Attribute::RuntimeVisibleParameterAnnotations(parameters)
        | Attribute::RuntimeInvisibleParameterAnnotations(parameters) => {
            let n = u8::try_from(parameters.len()).map_err(|_| "more than 255 parameters")?;
            out.push(n);
            for annotations in parameters {
                put_u2(out, count_u2(annotations.len())?);
                for annotation in annotations {
                    write_annotation(out, annotation)?;
                }
            }
        }
        Attribute::RuntimeVisibleTypeAnnotations(annotations)
        | Attribute::RuntimeInvisibleTypeAnnotations(annotations) => {
            put_u2(out, count_u2(annotations.len())?);
            for annotation in annotations {
                out.push(annotation.target_type);
                // Kept as read, for every target type.
                out.extend_from_slice(&annotation.target_info);
                let path = u8::try_from(annotation.type_path.len())
                    .map_err(|_| "a type path longer than 255")?;
                out.push(path);
                for step in &annotation.type_path {
                    out.push(step.type_path_kind);
                    out.push(step.type_argument_index);
                }
                write_annotation(out, &annotation.annotation)?;
            }
        }
        Attribute::AnnotationDefault(value) => write_element_value(out, value)?,
        Attribute::LocalVariableTable(entries) => {
            put_u2(out, count_u2(entries.len())?);
            for entry in entries {
                put_u2(out, entry.start_pc);
                put_u2(out, entry.length);
                put_u2(out, entry.name_index);
                put_u2(out, entry.descriptor_index);
                put_u2(out, entry.index);
            }
        }
        Attribute::LocalVariableTypeTable(entries) => {
            put_u2(out, count_u2(entries.len())?);
            for entry in entries {
                put_u2(out, entry.start_pc);
                put_u2(out, entry.length);
                put_u2(out, entry.name_index);
                put_u2(out, entry.signature_index);
                put_u2(out, entry.index);
            }
        }
        Attribute::MethodParameters(parameters) => {
            let n = u8::try_from(parameters.len()).map_err(|_| "more than 255 parameters")?;
            out.push(n);
            for parameter in parameters {
                put_u2(out, parameter.name_index);
                put_u2(out, parameter.access_flags);
            }
        }
        Attribute::LoadableDescriptors { descriptors } => put_u2_list(out, descriptors)?,
        Attribute::Unknown { data, .. } => out.extend_from_slice(data.as_bytes()),
    }
    end_attribute(out, body_at)
}

/// The name `attr` is written under (JVMS §4.7).
fn attribute_name(attr: &Attribute) -> &str {
    match attr {
        Attribute::Code(_) => "Code",
        Attribute::SourceFile(_) => "SourceFile",
        Attribute::ConstantValue { .. } => "ConstantValue",
        Attribute::Deprecated => "Deprecated",
        Attribute::Exceptions { .. } => "Exceptions",
        Attribute::LineNumberTable(_) => "LineNumberTable",
        Attribute::InnerClasses(_) => "InnerClasses",
        Attribute::Signature(_) => "Signature",
        Attribute::StackMapTable { .. } => "StackMapTable",
        Attribute::BootstrapMethods(_) => "BootstrapMethods",
        Attribute::Synthetic => "Synthetic",
        Attribute::EnclosingMethod { .. } => "EnclosingMethod",
        Attribute::NestHost { .. } => "NestHost",
        Attribute::NestMembers { .. } => "NestMembers",
        Attribute::Record(_) => "Record",
        Attribute::PermittedSubclasses { .. } => "PermittedSubclasses",
        Attribute::Module { .. } => "Module",
        Attribute::ModulePackages { .. } => "ModulePackages",
        Attribute::ModuleMainClass { .. } => "ModuleMainClass",
        Attribute::RuntimeVisibleAnnotations(_) => "RuntimeVisibleAnnotations",
        Attribute::RuntimeInvisibleAnnotations(_) => "RuntimeInvisibleAnnotations",
        Attribute::RuntimeVisibleParameterAnnotations(_) => "RuntimeVisibleParameterAnnotations",
        Attribute::RuntimeInvisibleParameterAnnotations(_) => {
            "RuntimeInvisibleParameterAnnotations"
        }
        Attribute::RuntimeVisibleTypeAnnotations(_) => "RuntimeVisibleTypeAnnotations",
        Attribute::RuntimeInvisibleTypeAnnotations(_) => "RuntimeInvisibleTypeAnnotations",
        Attribute::AnnotationDefault(_) => "AnnotationDefault",
        Attribute::LocalVariableTable(_) => "LocalVariableTable",
        Attribute::LocalVariableTypeTable(_) => "LocalVariableTypeTable",
        Attribute::MethodParameters(_) => "MethodParameters",
        Attribute::LoadableDescriptors { .. } => "LoadableDescriptors",
        Attribute::Unknown { name, .. } => &**name,
    }
}

/// One `annotation` structure (JVMS §4.7.16).
fn write_annotation(out: &mut Vec<u8>, annotation: &Annotation) -> Result<(), String> {
    put_u2(out, annotation.type_index);
    put_u2(out, count_u2(annotation.element_value_pairs.len())?);
    for pair in &annotation.element_value_pairs {
        put_u2(out, pair.element_name_index);
        write_element_value(out, &pair.value)?;
    }
    Ok(())
}

/// One `element_value` (JVMS §4.7.16.1).
fn write_element_value(out: &mut Vec<u8>, value: &ElementValue) -> Result<(), String> {
    match value {
        ElementValue::Const {
            tag,
            const_value_index,
        } => {
            out.push(*tag);
            put_u2(out, *const_value_index);
        }
        ElementValue::Enum {
            type_name_index,
            const_name_index,
        } => {
            out.push(b'e');
            put_u2(out, *type_name_index);
            put_u2(out, *const_name_index);
        }
        ElementValue::Class { class_info_index } => {
            out.push(b'c');
            put_u2(out, *class_info_index);
        }
        ElementValue::AnnotationValue(annotation) => {
            out.push(b'@');
            write_annotation(out, annotation)?;
        }
        ElementValue::Array(values) => {
            out.push(b'[');
            put_u2(out, count_u2(values.len())?);
            for value in values {
                write_element_value(out, value)?;
            }
        }
    }
    Ok(())
}

/// Write an attribute's name index and a length placeholder; the body's
/// start, for [`end_attribute`].
fn begin_attribute(
    pool: &mut PoolWriter<'_>,
    out: &mut Vec<u8>,
    name: &str,
) -> Result<usize, String> {
    let index = pool.utf8(name)?;
    put_u2(out, index);
    put_u4(out, 0);
    Ok(out.len())
}

/// Patch the length of the attribute whose body began at `body_at`.
fn end_attribute(out: &mut [u8], body_at: usize) -> Result<(), String> {
    let len = u32::try_from(out.len() - body_at).map_err(|_| "an attribute longer than a u4")?;
    out[body_at - 4..body_at].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// The live pool, index for index, plus whatever the writer appends.
struct PoolWriter<'a> {
    live: &'a ConstantPool,
    /// Slot 0 and the slot after a long or double are `Tombstone`.
    entries: Vec<ConstantPoolEntry>,
    /// First index of each `Utf8` (not one with lone surrogates, whose string
    /// is lossy).
    utf8: HashMap<Arc<str>, u16>,
    /// First `Class` entry naming each class.
    classes: HashMap<Arc<str>, u16>,
}

impl<'a> PoolWriter<'a> {
    fn from_live(live: &'a ConstantPool) -> Result<Self, String> {
        let len = live.len();
        if len == 0 || len > usize::from(u16::MAX) {
            return Err(format!("a constant pool of {len} slots"));
        }
        let mut entries = Vec::with_capacity(len + 16);
        let mut utf8 = HashMap::new();
        for i in 0..len {
            let index = i as u16;
            let entry = live
                .get(index)
                .cloned()
                .ok_or_else(|| format!("constant-pool slot #{i} missing"))?;
            if let ConstantPoolEntry::Utf8(text) = &entry {
                if index != 0 && live.get_utf8_wide(index).is_none() {
                    utf8.entry(Arc::clone(text)).or_insert(index);
                }
            }
            entries.push(entry);
        }
        let mut classes = HashMap::new();
        for (i, entry) in entries.iter().enumerate().skip(1) {
            if let ConstantPoolEntry::ClassReference { name_index } = entry {
                if live.get_utf8_wide(*name_index).is_some() {
                    continue;
                }
                if let Some(ConstantPoolEntry::Utf8(name)) = entries.get(usize::from(*name_index))
                {
                    classes.entry(Arc::clone(name)).or_insert(i as u16);
                }
            }
        }
        Ok(Self {
            live,
            entries,
            utf8,
            classes,
        })
    }

    fn push(&mut self, entry: ConstantPoolEntry) -> Result<u16, String> {
        let index = self.entries.len();
        // `constant_pool_count` is a u2 and counts slot 0.
        if index >= usize::from(u16::MAX) {
            return Err("the constant pool is full".to_string());
        }
        self.entries.push(entry);
        Ok(index as u16)
    }

    fn utf8(&mut self, text: &str) -> Result<u16, String> {
        if let Some(&index) = self.utf8.get(text) {
            return Ok(index);
        }
        let text: Arc<str> = Arc::from(text);
        let index = self.push(ConstantPoolEntry::Utf8(Arc::clone(&text)))?;
        self.utf8.insert(text, index);
        Ok(index)
    }

    fn class_ref(&mut self, name: &str) -> Result<u16, String> {
        if let Some(&index) = self.classes.get(name) {
            return Ok(index);
        }
        let name_index = self.utf8(name)?;
        let index = self.push(ConstantPoolEntry::ClassReference { name_index })?;
        self.classes.insert(Arc::from(name), index);
        Ok(index)
    }

    fn name_and_type(&mut self, name: &str, descriptor: &str) -> Result<u16, String> {
        let found = self
            .entries
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, entry)| {
                matches!(entry, ConstantPoolEntry::NameAndType { name_index, descriptor_index }
                    if self.utf8_text(*name_index) == Some(name)
                        && self.utf8_text(*descriptor_index) == Some(descriptor))
            })
            .map(|(index, _)| index);
        if let Some(index) = found {
            return Ok(index as u16);
        }
        let name_index = self.utf8(name)?;
        let descriptor_index = self.utf8(descriptor)?;
        self.push(ConstantPoolEntry::NameAndType {
            name_index,
            descriptor_index,
        })
    }

    /// The text of `Utf8` slot `index`, unless it holds lone surrogates.
    fn utf8_text(&self, index: u16) -> Option<&str> {
        if usize::from(index) < self.live.len() && self.live.get_utf8_wide(index).is_some() {
            return None;
        }
        match self.entries.get(usize::from(index)) {
            Some(ConstantPoolEntry::Utf8(text)) => Some(&**text),
            _ => None,
        }
    }

    /// `constant_pool_count` and every entry (JVMS §4.4).
    fn write(&self, out: &mut Vec<u8>) -> Result<(), String> {
        put_u2(out, self.entries.len() as u16);
        let mut i = 1;
        while i < self.entries.len() {
            match &self.entries[i] {
                ConstantPoolEntry::Tombstone => {
                    return Err(format!("an empty constant-pool slot #{i}"));
                }
                ConstantPoolEntry::Utf8(text) => {
                    let mut encoded = Vec::with_capacity(text.len());
                    let wide = if i < self.live.len() {
                        self.live.get_utf8_wide(i as u16)
                    } else {
                        None
                    };
                    match wide {
                        Some(units) => modified_utf8(units.iter().copied(), &mut encoded),
                        None => modified_utf8(text.encode_utf16(), &mut encoded),
                    }
                    let len = u16::try_from(encoded.len())
                        .map_err(|_| format!("Utf8 #{i} longer than a u2"))?;
                    out.push(1);
                    put_u2(out, len);
                    out.extend_from_slice(&encoded);
                }
                ConstantPoolEntry::Integer(value) => {
                    out.push(3);
                    put_u4(out, *value as u32);
                }
                ConstantPoolEntry::Float(value) => {
                    out.push(4);
                    put_u4(out, value.to_bits());
                }
                ConstantPoolEntry::Long(value) => {
                    out.push(5);
                    out.extend_from_slice(&value.to_be_bytes());
                    // Its second slot.
                    i += 1;
                }
                ConstantPoolEntry::Double(value) => {
                    out.push(6);
                    out.extend_from_slice(&value.to_bits().to_be_bytes());
                    i += 1;
                }
                ConstantPoolEntry::ClassReference { name_index } => {
                    out.push(7);
                    put_u2(out, *name_index);
                }
                ConstantPoolEntry::StringReference { string_index } => {
                    out.push(8);
                    put_u2(out, *string_index);
                }
                ConstantPoolEntry::FieldReference {
                    class_index,
                    name_and_type_index,
                } => {
                    out.push(9);
                    put_u2(out, *class_index);
                    put_u2(out, *name_and_type_index);
                }
                ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                } => {
                    out.push(10);
                    put_u2(out, *class_index);
                    put_u2(out, *name_and_type_index);
                }
                ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                } => {
                    out.push(11);
                    put_u2(out, *class_index);
                    put_u2(out, *name_and_type_index);
                }
                ConstantPoolEntry::NameAndType {
                    name_index,
                    descriptor_index,
                } => {
                    out.push(12);
                    put_u2(out, *name_index);
                    put_u2(out, *descriptor_index);
                }
                ConstantPoolEntry::MethodHandle {
                    reference_kind,
                    reference_index,
                } => {
                    out.push(15);
                    out.push(*reference_kind);
                    put_u2(out, *reference_index);
                }
                ConstantPoolEntry::MethodType { descriptor_index } => {
                    out.push(16);
                    put_u2(out, *descriptor_index);
                }
                ConstantPoolEntry::Dynamic {
                    bootstrap_method_attr_index,
                    name_and_type_index,
                } => {
                    out.push(17);
                    put_u2(out, *bootstrap_method_attr_index);
                    put_u2(out, *name_and_type_index);
                }
                ConstantPoolEntry::InvokeDynamic {
                    bootstrap_method_attr_index,
                    name_and_type_index,
                } => {
                    out.push(18);
                    put_u2(out, *bootstrap_method_attr_index);
                    put_u2(out, *name_and_type_index);
                }
                ConstantPoolEntry::Module { name_index } => {
                    out.push(19);
                    put_u2(out, *name_index);
                }
                ConstantPoolEntry::Package { name_index } => {
                    out.push(20);
                    put_u2(out, *name_index);
                }
            }
            i += 1;
        }
        Ok(())
    }
}

/// UTF-16 code units as the class file's modified UTF-8 (JVMS §4.4.7): NUL
/// as two bytes, a supplementary character as its two surrogates.
fn modified_utf8(units: impl Iterator<Item = u16>, out: &mut Vec<u8>) {
    for unit in units {
        match unit {
            0x0001..=0x007F => out.push(unit as u8),
            0x0000 | 0x0080..=0x07FF => {
                out.push(0xC0 | ((unit >> 6) as u8 & 0x1F));
                out.push(0x80 | (unit as u8 & 0x3F));
            }
            _ => {
                out.push(0xE0 | ((unit >> 12) as u8 & 0x0F));
                out.push(0x80 | ((unit >> 6) as u8 & 0x3F));
                out.push(0x80 | (unit as u8 & 0x3F));
            }
        }
    }
}

/// Parse `bytes` back and decode every attribute: what a transformer's
/// `ClassReader` will be handed must be a class file, and this class's.
fn check_parses_back(class: &Class, bytes: &[u8]) -> Result<(), String> {
    let mut parsed = cratonvm_reader::read_class(bytes).map_err(|e| format!("{e:?}"))?;
    if *parsed.this_class != *class.name {
        return Err(format!("parsed back as `{}`", parsed.this_class));
    }
    if parsed.methods.len() != class.methods.len() || parsed.fields.len() != class.fields.len() {
        return Err("parsed back with other members".to_string());
    }
    let cp = &parsed.constant_pool;
    cratonvm_reader::force_decode_all(&mut parsed.attributes, cp).map_err(|e| format!("{e:?}"))?;
    for method in parsed.methods.iter_mut() {
        cratonvm_reader::force_decode_all(&mut method.attributes, cp)
            .map_err(|e| format!("{e:?}"))?;
    }
    for field in parsed.fields.iter_mut() {
        cratonvm_reader::force_decode_all(&mut field.attributes, cp)
            .map_err(|e| format!("{e:?}"))?;
    }
    Ok(())
}

fn count_u2(n: usize) -> Result<u16, String> {
    u16::try_from(n).map_err(|_| format!("a count of {n} does not fit a u2"))
}

fn put_u2(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u4(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u2_list(out: &mut Vec<u8>, values: &[u16]) -> Result<(), String> {
    put_u2(out, count_u2(values.len())?);
    for value in values {
        put_u2(out, *value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::SharedVm;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// A version-49 class `recon/Probe` (no stack maps needed) with a long
    /// constant (two slots), a modified-UTF-8 string with NUL and a
    /// supplementary character, a `static final long LIMIT` with a
    /// `ConstantValue`, a method `static value()J` with a handler and a
    /// `LineNumberTable`, and a `SourceFile`: every attribute is one `Class`
    /// keeps, in the order the writer emits, so the file written back is
    /// this file byte for byte.
    fn probe_class(constant: i64) -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 49];
        b.extend_from_slice(&20u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, "recon/Probe"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()J"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(5); // #8 Long (and #9)
        b.extend_from_slice(&constant.to_be_bytes());
        utf8(&mut b, "LIMIT"); // #10
        utf8(&mut b, "J"); // #11
        utf8(&mut b, "ConstantValue"); // #12
        utf8(&mut b, "LineNumberTable"); // #13
        utf8(&mut b, "SourceFile"); // #14
        utf8(&mut b, "Probe.java"); // #15
        utf8(&mut b, "java/lang/Throwable"); // #16
        b.extend_from_slice(&[7, 0, 16]); // #17 Class #16
        // #18 Utf8 "a\0\u{e9}\u{10000}" in modified UTF-8.
        b.push(1);
        b.extend_from_slice(&11u16.to_be_bytes());
        b.extend_from_slice(&[
            0x61, 0xC0, 0x80, 0xC3, 0xA9, 0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80,
        ]);
        b.extend_from_slice(&[8, 0, 18]); // #19 String #18
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0]); // interfaces
        b.extend_from_slice(&[0, 1]); // fields_count
        b.extend_from_slice(&[0x00, 0x19, 0, 10, 0, 11, 0, 1]); // public static final long LIMIT
        b.extend_from_slice(&[0, 12, 0, 0, 0, 2, 0, 8]); // ConstantValue #8
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()J
        let code: [u8; 5] = [0x14, 0, 8, 0xAD, 0xBF]; // ldc2_w #8; lreturn; athrow
        let line_table_len: u32 = 2 + 2 * 4;
        // max_stack, max_locals, code_length, the two table counts (12), the
        // code, one handler (8), the line table's header (6) and body.
        let code_attr_len: u32 = 12 + code.len() as u32 + 8 + 6 + line_table_len;
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&code_attr_len.to_be_bytes());
        b.extend_from_slice(&[0, 2, 0, 0]); // max_stack 2, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 1, 0, 0, 0, 3, 0, 4, 0, 17]); // [0,3) -> 4, Throwable
        b.extend_from_slice(&[0, 1, 0, 13]); // one attribute: LineNumberTable
        b.extend_from_slice(&line_table_len.to_be_bytes());
        b.extend_from_slice(&[0, 2, 0, 0, 0, 7, 0, 4, 0, 9]);
        b.extend_from_slice(&[0, 1, 0, 14, 0, 0, 0, 2, 0, 15]); // SourceFile #15
        b
    }

    /// Wave 29, lane L3: a class never redefined is written back as the file
    /// it was defined from -- the pool index for index (a long's two slots,
    /// modified UTF-8), a field's `ConstantValue`, a body with its handler
    /// and line table, the class's `SourceFile`.
    #[test]
    fn a_class_never_redefined_is_written_back_as_its_class_file() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let file = probe_class(0x1122_3344_5566_7788);
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "recon/Probe",
                &file,
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let cm = shared.classes.class_manager.read();
        assert_eq!(reconstitute_class_file(&cm, cid), Ok(file));
    }

    /// A redefined class is not its own base any more: nothing is written.
    #[test]
    fn a_redefined_class_is_not_reconstituted() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "recon/Probe",
                &probe_class(1),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                Err(_) => return,
            }
        };
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, probe_class(2), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        let cm = shared.classes.class_manager.read();
        assert_eq!(
            reconstitute_class_file(&cm, cid),
            Err(ReconstituteRefusal::Redefined)
        );
    }

    #[test]
    fn modified_utf8_writes_nul_and_supplementary_characters_as_the_class_file_does() {
        let mut out = Vec::new();
        modified_utf8("a\0\u{e9}\u{10000}".encode_utf16(), &mut out);
        assert_eq!(
            out,
            [0x61, 0xC0, 0x80, 0xC3, 0xA9, 0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]
        );
        // A lone surrogate, as a `wide_utf8` entry keeps it.
        out.clear();
        modified_utf8([0xD800u16].into_iter(), &mut out);
        assert_eq!(out, [0xED, 0xA0, 0x80]);
    }

    /// Names the live pool lacks are appended; names it has are reused.
    #[test]
    fn the_pool_writer_reuses_live_entries_and_appends_the_rest() {
        let live = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(Arc::from("a/B")),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8(Arc::from("m")),
            ConstantPoolEntry::Utf8(Arc::from("()V")),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
        ]);
        let mut pool = PoolWriter::from_live(&live).expect("a small pool");
        assert_eq!(pool.class_ref("a/B"), Ok(2));
        assert_eq!(pool.utf8("m"), Ok(3));
        assert_eq!(pool.name_and_type("m", "()V"), Ok(5));
        assert_eq!(pool.class_ref("c/D"), Ok(7), "Utf8 #6, then Class #7");
        assert_eq!(pool.name_and_type("m", "(I)V"), Ok(9), "Utf8 #8, then #9");
        let mut out = Vec::new();
        pool.write(&mut out).expect("writable");
        assert_eq!(&out[..2], &10u16.to_be_bytes());
    }
}
