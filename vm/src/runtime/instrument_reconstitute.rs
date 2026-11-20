// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A class file written back from a live class, for `retransformClasses` when
//! no retransformation base survives anywhere else.
//!
//! Interpreter round i1 wave 29, lane L3
//! (`docs/feature-designs/real-jdk-mode.md`).
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

/// A cache/resource copy is not HotSpot's cached agent base: without an
/// agent-owned base HotSpot reconstitutes parser-stored flags. Preserve all
/// other bytes and CP indexes here, including attributes our live writer does
/// not retain. Agent-pinned bases are authoritative and remain byte-for-byte.
pub(crate) fn normalize_source_flags(cm: &ClassManager, id: ClassId, mut bytes: Vec<u8>) -> Vec<u8> {
    if cm.has_raw_agent_base(id) {
        return bytes;
    }
    let Some(class) = cm.get_class(id) else { return bytes; };
    let Ok(parsed) = cratonvm_reader::read_class(&bytes) else { return bytes; };
    let mut at = 10usize;
    let mut index = 1usize;
    while index < parsed.constant_pool.len() {
        let Some(&tag) = bytes.get(at) else { return bytes; };
        at += 1;
        let (length, slots) = match tag {
            1 => {
                let Some(raw) = bytes.get(at..at+2) else { return bytes; };
                (2 + u16::from_be_bytes([raw[0], raw[1]]) as usize, 1)
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => (4, 1),
            5 | 6 => (8, 2),
            7 | 8 | 16 | 19 | 20 => (2, 1),
            15 => (3, 1),
            _ => return bytes,
        };
        at += length;
        index += slots;
    }
    if let Some(flags) = bytes.get_mut(at..at+2) {
        flags.copy_from_slice(&class.access_flags.bits().to_be_bytes());
    }
    let count_at = parsed.attributes.iter().filter_map(|a| match a {
        LazyAttribute::Raw(raw) => raw.range.start.checked_sub(8), _ => None,
    }).min();
    let mut remove = Vec::new();
    for attr in &parsed.attributes {
        let LazyAttribute::Raw(raw) = attr else { continue; };
        if &*raw.name != "InnerClasses" { continue; }
        if class.inner_classes.is_empty() {
            remove.push(raw.range.start-6..raw.range.end);
            continue;
        }
        // Interpreter round i7 wave 136, lane L3: each row of the base takes
        // the stored flags of the live row naming the same inner class (its
        // outer class and simple name too, when they tell two rows apart),
        // never the live row at the same position. The base may be a
        // resource or loader copy whose table is not the live one row for
        // row (the live table drops rows whose inner class does not resolve,
        // `NestingAttributes::absorb`), and a live table longer than the
        // base's wrote past the attribute into the next one. A row with no
        // live counterpart is left as it was. The live flags already carry
        // the version-dependent masking.
        let Some(raw_rows) = bytes.get(raw.range.start..raw.range.start + 2) else { continue; };
        let rows = usize::from(u16::from_be_bytes([raw_rows[0], raw_rows[1]]));
        if raw.range.start + 2 + 8 * rows > raw.range.end { continue; }
        for n in 0..rows {
            let row = raw.range.start + 2 + 8 * n;
            let u2 = |bytes: &[u8], at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
            let (inner, outer, simple) = (u2(&bytes[..], row), u2(&bytes[..], row + 2), u2(&bytes[..], row + 4));
            let Some(inner) = parsed.constant_pool.get_class_name(inner) else { continue; };
            let outer = if outer == 0 { "" } else { parsed.constant_pool.get_class_name(outer).unwrap_or("") };
            let simple = if simple == 0 { "" } else { parsed.constant_pool.get_utf8(simple).unwrap_or("") };
            let exact = class.inner_classes.iter().find(|e| {
                &*e.inner_class == inner && &*e.outer_class == outer && &*e.inner_name == simple
            });
            let entry = exact.or_else(|| class.inner_classes.iter().find(|e| &*e.inner_class == inner));
            let Some(entry) = entry else { continue; };
            bytes[row + 6..row + 8].copy_from_slice(&entry.access_flags.to_be_bytes());
        }
    }
    if let Some(count_at) = count_at.filter(|_| !remove.is_empty()) {
        // The table's own count, not the reader's attribute list length.
        let declared = u16::from_be_bytes([bytes[count_at], bytes[count_at + 1]]);
        let count = declared.saturating_sub(remove.len() as u16);
        bytes[count_at..count_at+2].copy_from_slice(&count.to_be_bytes());
        for range in remove.into_iter().rev() { bytes.drain(range); }
    }
    bytes
}

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
    let dbg = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok();
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
            LazyAttribute::Raw(raw) => {
                let (name, range) = (&raw.name, &raw.range);
                let buffer: &[u8] = raw.source.as_ref();
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
        Attribute::Module(module) => {
            let cratonvm_reader::attribute::ModuleAttribute {
                name_index,
                flags,
                version_index,
                requires,
                exports,
                opens,
                uses,
                provides,
            } = &**module;
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

/// SBR wave 5 (reflmeta): the body bytes of `lazy` when it is the annotation
/// attribute named `name` (`RuntimeVisibleAnnotations`,
/// `RuntimeVisibleParameterAnnotations`, `RuntimeVisibleTypeAnnotations` or
/// `AnnotationDefault`), for the raw byte fields HotSpot gives reflection
/// objects. A still-raw attribute hands back its bytes as read; a decoded one
/// is written back with the writers above, which keep every constant pool
/// index as read, so the bytes index the live pool either way.
pub(crate) fn annotation_attribute_body(lazy: &LazyAttribute, name: &str) -> Option<Vec<u8>> {
    match lazy {
        LazyAttribute::Raw(raw) => {
            if &*raw.name != name {
                return None;
            }
            let buffer: &[u8] = raw.source.as_ref();
            buffer.get(raw.range.clone()).map(<[u8]>::to_vec)
        }
        // An annotation attribute the reader could not decode is kept opaque
        // (HotSpot reads it as bytes; interpreter round i5 wave 111, lane
        // L5): its bytes as read, for reflection's `AnnotationParser` to
        // judge (`AnnotationFormatError`), as on HotSpot.
        LazyAttribute::Decoded(Attribute::Unknown { name: unknown, data }) => {
            (&**unknown == name).then(|| data.as_bytes().to_vec())
        }
        LazyAttribute::Decoded(attr) => {
            let matches = matches!(
                (attr, name),
                (
                    Attribute::RuntimeVisibleAnnotations(_),
                    "RuntimeVisibleAnnotations"
                ) | (
                    Attribute::RuntimeVisibleParameterAnnotations(_),
                    "RuntimeVisibleParameterAnnotations"
                ) | (
                    Attribute::RuntimeVisibleTypeAnnotations(_),
                    "RuntimeVisibleTypeAnnotations"
                ) | (Attribute::AnnotationDefault(_), "AnnotationDefault")
            );
            if !matches {
                return None;
            }
            cratonvm_reader::attribute::annotation_attribute_body(attr)
        }
    }
}

/// SBR wave 5 (reflmeta): a `RuntimeVisibleAnnotations` body for a class's
/// kept (visible-only) annotations, for `Class.getRawAnnotations`. `None` for
/// none, HotSpot's `null`.
pub(crate) fn class_annotations_body(annotations: &[Annotation]) -> Option<Vec<u8>> {
    if annotations.is_empty() {
        return None;
    }
    cratonvm_reader::attribute::annotation_attribute_body(&Attribute::RuntimeVisibleAnnotations(
        annotations.to_vec(),
    ))
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
                    utf8.entry(text.to_arc()).or_insert(index);
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
                if let Some(ConstantPoolEntry::Utf8(name)) = entries.get(usize::from(*name_index)) {
                    classes.entry(name.to_arc()).or_insert(i as u16);
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
        let index = self.push(ConstantPoolEntry::Utf8(Arc::clone(&text).into()))?;
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
                    out.extend_from_slice(&value.get().to_be_bytes());
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
mod tests;
