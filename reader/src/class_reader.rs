// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Class file reader — parses raw bytes into a [`ClassFile`] structure.
//!
//! Reference: <https://docs.oracle.com/javase/specs/jvms/se21/html/jvms-4.html>

use crate::attribute::*;
use crate::buffer::ClassFileBuffer;
use crate::byte_view::SharedBytes;
use crate::class_access_flags::*;
use crate::class_file::ClassFile;
use crate::class_file_version::ClassFileVersion;
use crate::class_reader_error::ClassReaderError;
use crate::constant_pool::{ConstantPool, ConstantPoolEntry};
use crate::field::ClassFileField;
// Resource limits and the checked-arithmetic helpers that enforce them all
// live in one place — see `reader/src/limits.rs` and
// `docs/security/reader/limits.md` for the inventory.
use crate::limits::{
    bounded_capacity, checked_end, ensure_count_fits, wire_len_to_usize,
    MIN_CONSTANT_POOL_ENTRY_BYTES,
};
use crate::method::ClassFileMethod;
use std::sync::Arc;
use tracing::{debug, trace};

const CLASS_FILE_MAGIC: u32 = 0xCAFEBABE;

/// Start of the [`ClassReaderError::InvalidClassData`] message for bytes after
/// the class attributes table; the class's internal name follows. It is
/// HotSpot's `ClassFormatError` text verbatim, so a caller that sees a message
/// with this prefix raises it as is.
pub const EXTRA_BYTES_PREFIX: &str = "Extra bytes at the end of class file ";

/// Maximum count for constant pool, methods, fields, interfaces, exception
/// table entries, and attributes.  Per JVM spec these are u16 fields, so the
/// hard upper bound is 65 535.  We validate against this limit *before*
/// allocating to prevent a crafted class file with huge counts from causing
/// an out-of-memory denial-of-service.
const MAX_CP_SIZE: u16 = crate::limits::MAX_CONSTANT_POOL_COUNT; // 65 535 — JVM spec §4.1
const MAX_FIELD_COUNT: u16 = u16::MAX;
const MAX_METHOD_COUNT: u16 = u16::MAX;
const MAX_INTERFACE_COUNT: u16 = u16::MAX;
const MAX_ATTRIBUTE_COUNT: u16 = u16::MAX;
// MAX_EXCEPTION_TABLE_COUNT was used by the eager `Code` parser; that parser
// now lives in `attribute.rs` (called on-demand via `LazyAttribute::decode`),
// so the constant is no longer needed here.

// Smallest possible on-the-wire size of one entry in each of the top-level
// tables. These are the multipliers that turn a declared count into "the
// input is too short to contain this table", so preallocation is bounded by
// the *file*, not by a number typed into a two-byte field.
//
//   interfaces[]  — one u2 constant-pool index          (JVMS §4.1)
//   field_info    — access_flags, name_index,
//                   descriptor_index, attributes_count  (JVMS §4.5)
//   method_info   — same four u2 fields                 (JVMS §4.6)
//   attribute_info— attribute_name_index (u2) +
//                   attribute_length (u4)               (JVMS §4.7)
const INTERFACE_ENTRY_BYTES: usize = 2;
const FIELD_ENTRY_BYTES: usize = 8;
const METHOD_ENTRY_BYTES: usize = 8;
const ATTRIBUTE_HEADER_BYTES: usize = 6;

/// A declared count the remaining bytes cannot hold
/// ([`ensure_count_fits`]'s refusal): HotSpot reads on and runs off the end,
/// `Truncated class file` (interpreter round i5 wave 109, lane L5).
fn truncated(e: ClassReaderError) -> ClassReaderError {
    ClassReaderError::class_file_parser(crate::class_reader_error::TRUNCATED_CLASS_FILE, e)
}

/// Whose attribute table [`read_attributes`] reads: HotSpot words a bad
/// attribute name index differently for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttributeOwner {
    Class,
    /// A class whose superclass is `java/lang/Record`: HotSpot reads a
    /// `Record` attribute only there ("Skip over Record attribute if ...
    /// super class is not java.lang.Record"; wave 112, L5).
    RecordClass,
    /// A module descriptor's (`ACC_MODULE`) class attributes: never defined
    /// as a class, read by the module system, every attribute as before.
    ModuleInfo,
    Field,
    /// A field with `ACC_STATIC` (HotSpot reads `ConstantValue` only there).
    StaticField,
    Method,
}

impl AttributeOwner {
    /// `ClassFileParser`'s refusal of an `attribute_name_index` that is not a
    /// `CONSTANT_Utf8` (JDK 25: `parse_classfile_attributes`,
    /// `parse_field_attributes`, `parse_method`), without the class tail.
    fn bad_name_index_text(self, index: u16) -> String {
        match self {
            Self::Class | Self::RecordClass | Self::ModuleInfo => {
                format!("Attribute name has bad constant pool index {index}")
            }
            Self::Field | Self::StaticField => format!("Invalid field attribute index {index}"),
            Self::Method => format!("Invalid method attribute name index {index}"),
        }
    }
}

/// Does HotSpot 25's `ClassFileParser` read an attribute named `name` in this
/// table of a version-`major` class file (`parse_classfile_attributes`,
/// `parse_field_attributes`, `parse_method`)? Any other attribute is skipped
/// there whatever its body, so the reader must not refuse it either
/// ([`LazyAttribute::new_raw_lenient_in`]). A module descriptor's class attributes are all
/// read as before ([`AttributeOwner::ModuleInfo`]): it is never defined as a
/// class, and the module system reads it through this parser; in an ordinary
/// class `Module`, `ModuleMainClass`, ... are skipped as HotSpot skips them. Interpreter round i5 wave 111, lane
/// L5 (probe `L5W111ClassFileParserLast`, rows `ignored-*`).
fn hotspot_reads_attribute(owner: AttributeOwner, name: &str, major: u16) -> bool {
    if owner == AttributeOwner::ModuleInfo {
        return true;
    }
    let since_49 = major >= 49;
    // The annotation attributes are skipped by `ClassFileParser` as bytes
    // (the invisible ones always, the visible ones parsed only leniently for
    // the VM's own annotations; a malformed body is reflection's
    // `AnnotationFormatError` later, not a refusal), so they go through
    // the lenient path in every table: decoded when they decode, opaque
    // otherwise.
    let signature = name == "Signature";
    match owner {
        AttributeOwner::Class | AttributeOwner::RecordClass => match name {
            "SourceFile" | "SourceDebugExtension" | "InnerClasses" | "Synthetic" | "Deprecated" => {
                true
            }
            "EnclosingMethod" => since_49,
            "BootstrapMethods" => major >= 51,
            "NestHost" | "NestMembers" => major >= 55,
            "Record" => major >= 60,
            "PermittedSubclasses" => major >= 61,
            _ => signature && since_49,
        },
        AttributeOwner::Field | AttributeOwner::StaticField => match name {
            "ConstantValue" => owner == AttributeOwner::StaticField,
            "Synthetic" | "Deprecated" => true,
            _ => signature && since_49,
        },
        AttributeOwner::Method => match name {
            "Code" | "Exceptions" | "MethodParameters" | "Synthetic" | "Deprecated" => true,
            _ => signature && since_49,
        },
        AttributeOwner::ModuleInfo => true,
    }
}

/// Validate that a section count does not exceed the given limit.
fn validate_count(label: &str, count: u16, limit: u16) -> Result<(), ClassReaderError> {
    if count > limit {
        return Err(ClassReaderError::InvalidClassData {
            message: format!("{label} count {count} exceeds maximum allowed value {limit}"),
        });
    }
    Ok(())
}

/// Parse a `.class` file from a byte slice.
///
/// This copies `data` once into a fresh `Arc<[u8]>` and delegates to
/// [`read_class_arc`]. Callers that already hold the class bytes as an
/// `Arc<[u8]>` should call [`read_class_arc`] directly to skip that copy.
pub fn read_class(data: &[u8]) -> Result<ClassFile, ClassReaderError> {
    read_class_arc(Arc::from(data))
}

/// Parse a `.class` file from an already-`Arc`-wrapped byte buffer.
///
/// The `source` `Arc<[u8]>` is threaded through every `LazyAttribute::Raw`
/// produced for this class file. Each lazy attribute is then a `(name:
/// Arc<str>, source: Arc<[u8]>, range: Range<usize>)` — a refcount bump on
/// the source plus a range, no body memcpy. The Arc is kept alive by
/// whichever lazy attribute(s) survive parsing; once they decode or drop,
/// the backing buffer is freed.
///
/// Accepting an `Arc<[u8]>` directly avoids the O(class_file_size) copy
/// that [`read_class`] performs at parse entry when the caller already
/// owns a shared buffer.
pub fn read_class_arc(source: Arc<[u8]>) -> Result<ClassFile, ClassReaderError> {
    read_class_shared(source.into())
}

/// Parse a class directly from shared owned or externally-backed bytes.
///
/// File-mapped stored JAR entries use this entry point so every lazy
/// attribute and bytecode view retains the archive mapping rather than
/// materializing a per-class copy.
///
/// Preview: the plain entry points ([`read_class`], [`read_class_arc`], this
/// one) parse a preview class file of this release (`69.65535`) as a shape
/// the reader understands, `verify(true)`; every other version verdict is
/// unchanged. Whether the VM may DEFINE it is the VM's own `--enable-preview`
/// bit, which the class manager applies to every define
/// (`ClassManager::define_class_shared_parsed`, its own parse through
/// [`read_class_shared_with_preview`]). Interpreter round i5 wave 101, lane
/// L5: the bit was a process global read here, which only the launcher set.
pub fn read_class_shared(source: SharedBytes) -> Result<ClassFile, ClassReaderError> {
    read_class_shared_with_preview(source, true)
}

/// [`read_class`] for a VM whose `--enable-preview` bit is `preview_enabled`:
/// a preview class file is refused with
/// [`VersionRejection::PreviewNotEnabled`](crate::VersionRejection) when it is
/// off, at the point HotSpot's `ClassFileParser::verify_class_version` refuses
/// it (before the constant pool).
pub fn read_class_with_preview(
    data: &[u8],
    preview_enabled: bool,
) -> Result<ClassFile, ClassReaderError> {
    read_class_shared_with_preview(Arc::<[u8]>::from(data).into(), preview_enabled)
}

/// [`read_class_shared`] for a VM whose `--enable-preview` bit is
/// `preview_enabled` (see [`read_class_with_preview`]).
pub fn read_class_shared_with_preview(
    source: SharedBytes,
    preview_enabled: bool,
) -> Result<ClassFile, ClassReaderError> {
    let mut buf = ClassFileBuffer::new(source.as_ref());

    // Magic number
    let magic = buf.read_u32()?;
    if magic != CLASS_FILE_MAGIC {
        return Err(ClassReaderError::InvalidMagicNumber { magic });
    }

    // Version
    let minor = buf.read_u16()?;
    let major = buf.read_u16()?;
    let version = ClassFileVersion::new(major, minor);
    // Preview gating (JVMS 4.1). The enablement bit is the caller's VM's
    // (`preview_enabled`; the plain entry points pass `true`, see
    // `read_class_shared`).
    //
    // Blast radius, stated because this runs on every class the VM loads: the
    // *only* byte pattern whose verdict moves is `minor == 0xFFFF` together with
    // `major == MAX_SUPPORTED.major` (69 today) — bytes 4..7 of the file reading
    // `FF FF 00 45`. That pattern used to load unconditionally and now needs
    // preview enabled. Every other pattern keeps its previous answer
    // bit-for-bit, structurally rather than by inspection: `is_supported` is now
    // `verify(true)`, and the only substitution here is `true` ->
    // `preview_enabled()`, which those two differ on in exactly that one arm.
    //
    // The JDK's own class files cannot reach the new arm. They are `69.0` —
    // measured on Adoptium 25.0.3.9, including `StructuredTaskScope.class`
    // itself, because a preview *API* does not imply a preview class file: the
    // `@PreviewFeature` annotation is javac's business and leaves no mark in the
    // file. `verify` returns `Ok` for `minor == 0` two branches before the
    // preview arm exists.
    if let Err(rejection) = version.verify(preview_enabled) {
        return Err(ClassReaderError::UnsupportedVersion {
            major,
            minor,
            rejection,
        });
    }
    debug!("Class file version: {version}");

    // Constant pool
    let constant_pool = read_constant_pool(&mut buf, major)?;
    debug!("Constant pool: {} entries", constant_pool.len());

    // Access flags, this class, super class
    let access_flags_raw = buf.read_u16()?;
    let access_flags = ClassAccessFlags::from_bits_retain(access_flags_raw);
    let class_owner = if access_flags_raw & 0x8000 != 0 {
        AttributeOwner::ModuleInfo
    } else {
        AttributeOwner::Class
    };

    // Resolve `this_class` / `super_class` / interface names directly to
    // the pool-interned `Arc<str>` rather than allocating fresh Strings.
    // The constant pool already holds the canonical interned form of every
    // Utf8 entry, so each name becomes a single refcount bump on the
    // shared backing allocation. On bootstrap (~5 k classes, ~12 names
    // per class on average) this eliminates ~60 k throwaway String allocs.
    // HotSpot's `ClassFileParser` texts for the refusals below (interpreter
    // round i5 wave 109, lane L5): `this_class` is checked while HotSpot's
    // `_class_name` is still the requested name; every refusal after it names
    // `this_class` (`named`).
    let this_class_index = buf.read_u16()?;
    let this_class = constant_pool
        .get_class_name_arc(this_class_index)
        .ok_or_else(|| {
            ClassReaderError::class_file_parser(
                format!("Invalid this class index {this_class_index} in constant pool"),
                ClassReaderError::InvalidConstantPool {
                    index: this_class_index,
                    message: "this_class must reference a valid Class entry".to_string(),
                },
            )
        })?;
    debug!("Parsing class: {this_class}");
    let named = |e: ClassReaderError| e.naming_class(&this_class);

    let super_class_index = buf.read_u16()?;
    let super_class = if super_class_index == 0 {
        None // java.lang.Object has no superclass
    } else {
        Some(
            constant_pool
                .get_class_name_arc(super_class_index)
                .ok_or_else(|| {
                    named(ClassReaderError::class_file_parser(
                        format!("Invalid superclass index {super_class_index}"),
                        ClassReaderError::InvalidConstantPool {
                            index: super_class_index,
                            message: "super_class must reference a valid Class entry".to_string(),
                        },
                    ))
                })?,
        )
    };

    // Interfaces
    let interfaces_count = buf.read_u16()?;
    validate_count("interfaces", interfaces_count, MAX_INTERFACE_COUNT)?;
    // Reject a declared count the remaining bytes cannot possibly hold
    // BEFORE reserving for it — see `limits::ensure_count_fits`.
    ensure_count_fits(
        "interfaces",
        interfaces_count as usize,
        INTERFACE_ENTRY_BYTES,
        buf.remaining(),
    )
    .map_err(truncated)?;
    let mut interfaces: Vec<Arc<str>> = Vec::with_capacity(bounded_capacity(
        interfaces_count as usize,
        INTERFACE_ENTRY_BYTES,
        buf.remaining(),
    ));
    for _ in 0..interfaces_count {
        let iface_index = buf.read_u16()?;
        let iface_name = constant_pool
            .get_class_name_arc(iface_index)
            .ok_or_else(|| {
                named(ClassReaderError::class_file_parser(
                    format!("Interface name has bad constant pool index {iface_index}"),
                    ClassReaderError::InvalidConstantPool {
                        index: iface_index,
                        message: "interface must reference a valid Class entry".to_string(),
                    },
                ))
            })?;
        interfaces.push(iface_name);
    }

    // Fields
    let fields_count = buf.read_u16()?;
    validate_count("fields", fields_count, MAX_FIELD_COUNT)?;
    ensure_count_fits(
        "fields",
        fields_count as usize,
        FIELD_ENTRY_BYTES,
        buf.remaining(),
    )
    .map_err(truncated)?;
    let mut fields = Vec::with_capacity(bounded_capacity(
        fields_count as usize,
        FIELD_ENTRY_BYTES,
        buf.remaining(),
    ));
    for _ in 0..fields_count {
        fields.push(read_field(&mut buf, &constant_pool, &source, major).map_err(named)?);
    }

    // Methods
    let methods_count = buf.read_u16()?;
    validate_count("methods", methods_count, MAX_METHOD_COUNT)?;
    ensure_count_fits(
        "methods",
        methods_count as usize,
        METHOD_ENTRY_BYTES,
        buf.remaining(),
    )
    .map_err(truncated)?;
    let mut methods = Vec::with_capacity(bounded_capacity(
        methods_count as usize,
        METHOD_ENTRY_BYTES,
        buf.remaining(),
    ));
    for _ in 0..methods_count {
        methods.push(read_method(&mut buf, &constant_pool, &source, major).map_err(named)?);
    }

    // Class attributes
    let class_owner = match class_owner {
        AttributeOwner::Class if super_class.as_deref() == Some("java/lang/Record") => {
            AttributeOwner::RecordClass
        }
        other => other,
    };
    let attributes =
        read_attributes(&mut buf, &constant_pool, &source, class_owner, major).map_err(named)?;
    if buf.remaining() != 0 {
        // HotSpot's `ClassFileParser::parse_stream` wording
        // (`guarantee_property(stream->at_eos(), "Extra bytes at the end of
        // class file %s")`), naming the class from `this_class` in internal
        // form; `ClassManager` lifts this exact text into the
        // `ClassFormatError` (interpreter round i4 wave 98, lane L5;
        // `tools/probes/interp/L5/L5W98ClassFileParseTexts.java`).
        return Err(ClassReaderError::InvalidClassData {
            message: format!("{EXTRA_BYTES_PREFIX}{this_class}"),
        });
    }

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

/// Decode Java *modified UTF-8* bytes into UTF-16 code units, tolerating
/// **lone surrogates**.
///
/// `cesu8::from_java_cesu8` rejects unpaired surrogates because the resulting
/// code point is not a valid Unicode scalar value (a Rust `str` cannot hold
/// it). This decoder instead emits every 3-byte group as its raw 16-bit value,
/// so an unpaired surrogate (U+D800..U+DFFF) round-trips verbatim and a
/// surrogate *pair* becomes two units — which is exactly the UTF-16 the JDK
/// keeps in a `String`'s `char[]`. ANTLR-generated `_serializedATN` string
/// constants are the common producer of lone surrogates.
///
/// Returns `Err(())` on genuinely malformed modified UTF-8 (bad continuation
/// bytes, truncated sequences, a bare `0x00`, or a 4-byte sequence — which
/// modified UTF-8 never uses).
fn decode_java_mutf8_to_utf16(bytes: &[u8]) -> Result<Vec<u16>, ()> {
    let mut out: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b0 = bytes[i];
        if b0 == 0x00 {
            // Modified UTF-8 forbids a bare NUL — it is encoded as 0xC0 0x80.
            return Err(());
        } else if b0 < 0x80 {
            out.push(b0 as u16);
            i += 1;
        } else if b0 & 0xE0 == 0xC0 {
            // 2-byte: 110x xxxx  10xx xxxx  (also encodes NUL as 0xC0 0x80)
            if i + 1 >= bytes.len() {
                return Err(());
            }
            let b1 = bytes[i + 1];
            if b1 & 0xC0 != 0x80 {
                return Err(());
            }
            out.push((((b0 as u16) & 0x1F) << 6) | ((b1 as u16) & 0x3F));
            i += 2;
        } else if b0 & 0xF0 == 0xE0 {
            // 3-byte: 1110 xxxx  10xx xxxx  10xx xxxx  (the surrogate range too)
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let b1 = bytes[i + 1];
            let b2 = bytes[i + 2];
            if b1 & 0xC0 != 0x80 || b2 & 0xC0 != 0x80 {
                return Err(());
            }
            out.push(
                (((b0 as u16) & 0x0F) << 12) | (((b1 as u16) & 0x3F) << 6) | ((b2 as u16) & 0x3F),
            );
            i += 3;
        } else {
            // 4-byte UTF-8 is not valid modified UTF-8.
            return Err(());
        }
    }
    Ok(out)
}

/// A `CONSTANT_Long` / `CONSTANT_Double` in the last pool slot: HotSpot
/// `parse_constant_pool_entries`' `Invalid constant pool entry <i> in class
/// file C` (interpreter round i5 wave 109, lane L5).
fn last_slot_category2(index: u16, what: &str) -> ClassReaderError {
    ClassReaderError::class_file_parser(
        format!("Invalid constant pool entry {index}"),
        ClassReaderError::InvalidConstantPool {
            index,
            message: format!("{what} must not occupy the last constant pool slot"),
        },
    )
}

/// HotSpot `ClassFileParser::parse_constant_pool`'s second pass over the
/// entries a class file names other entries with, in pool order, for the two
/// kinds HotSpot checks in every loader: a `CONSTANT_Class`'s name and a
/// `CONSTANT_String`'s value must each be a `CONSTANT_Utf8`, else `Invalid
/// constant pool index <n> in class file C` (`valid_symbol_at`). Before
/// interpreter round i5 wave 109 (lane L5) the reader kept a dangling one,
/// and a `this_class` naming it read as "this_class must reference a valid
/// Class entry". The `_need_verify`-only member-reference and `NameAndType`
/// checks are not made here (see
/// `docs/feature-designs/real-jdk-mode.md`).
fn check_constant_pool_symbol_refs(entries: &[ConstantPoolEntry]) -> Result<(), ClassReaderError> {
    for (index, entry) in entries.iter().enumerate().skip(1) {
        let referenced = match entry {
            ConstantPoolEntry::ClassReference { name_index } => *name_index,
            ConstantPoolEntry::StringReference { string_index } => *string_index,
            _ => continue,
        };
        if !matches!(
            entries.get(usize::from(referenced)),
            Some(ConstantPoolEntry::Utf8(_))
        ) {
            return Err(ClassReaderError::class_file_parser(
                format!("Invalid constant pool index {referenced}"),
                ClassReaderError::InvalidConstantPool {
                    index: u16::try_from(index).unwrap_or(u16::MAX),
                    message: format!(
                        "constant pool entry names index {referenced}, which is not a CONSTANT_Utf8 entry"
                    ),
                },
            ));
        }
    }
    // HotSpot `parse_constant_pool`'s later pass: a `CONSTANT_Methodref`
    // whose name starts with `<` must name `<init>`, else `Bad method name at
    // constant pool index <name Utf8 index> in class file C` (interpreter
    // round i5 wave 109 hand-back, lane L5; page
    // `i109-L7-methodref-named-clinit-passes-the-parser`, probe
    // `L7W109OldVerifierTexts` row `methodref-clinit`). HotSpot makes it under
    // `_need_verify`; the boot and platform classes never trip it. A reference
    // whose `NameAndType` is malformed is left to resolution, as before.
    for entry in entries.iter().skip(1) {
        let ConstantPoolEntry::MethodReference {
            name_and_type_index,
            ..
        } = entry
        else {
            continue;
        };
        let Some(ConstantPoolEntry::NameAndType { name_index, .. }) =
            entries.get(usize::from(*name_and_type_index))
        else {
            continue;
        };
        if let Some(ConstantPoolEntry::Utf8(name)) = entries.get(usize::from(*name_index)) {
            if name.starts_with('<') && &**name != "<init>" {
                return Err(ClassReaderError::class_file_parser(
                    format!("Bad method name at constant pool index {name_index}"),
                    ClassReaderError::InvalidConstantPool {
                        index: *name_index,
                        message: format!("a CONSTANT_Methodref names the method {name}"),
                    },
                ));
            }
        }
    }
    Ok(())
}

/// `major` is the class file's major version (the tags it may hold).
fn read_constant_pool(buf: &mut ClassFileBuffer, major: u16) -> Result<ConstantPool, ClassReaderError> {
    let count = buf.read_u16()?;
    // JVMS §4.1: constant_pool_count must be >= 1. The 0-th slot is a
    // reserved sentinel; valid entries live at indices 1..count. HotSpot
    // rejects count == 0 with java.lang.ClassFormatError.
    if count == 0 {
        // HotSpot `parse_stream`'s text (interpreter round i5 wave 109, lane L5).
        return Err(ClassReaderError::class_file_parser(
            "Illegal constant pool size 0",
            ClassReaderError::InvalidConstantPool {
                index: 0,
                message: "constant_pool_count must be >= 1".to_string(),
            },
        ));
    }
    validate_count("constant_pool", count, MAX_CP_SIZE)?;
    // `count` is a u16, so it is bounded at 65 535 — but 65 535
    // `ConstantPoolEntry`s is still a multi-megabyte reservation that a
    // *two-byte* header can demand from a file with nothing after it. The
    // pool holds `count - 1` real entries (slot 0 is the reserved
    // sentinel), and every `cp_info` costs at least
    // `MIN_CONSTANT_POOL_ENTRY_BYTES` per slot it covers, so a declared
    // count needing more bytes than the file has left is provably a lie.
    // Reject it here rather than after the allocator has been asked for
    // the memory. The bound is conservative — `buf.remaining()` still
    // includes the post-pool sections, so no legal class file is refused.
    ensure_count_fits(
        "constant_pool",
        (count as usize) - 1,
        MIN_CONSTANT_POOL_ENTRY_BYTES,
        buf.remaining(),
    )
    .map_err(truncated)?;
    // With that check passed, `count` is itself bounded by the input
    // length, so reserving the exact size is safe and avoids reallocations
    // while parsing large classes.
    let mut entries: Vec<ConstantPoolEntry> = Vec::with_capacity(count as usize);
    entries.push(ConstantPoolEntry::Tombstone); // Index 0

    // Side table of exact UTF-16 units for the rare surrogate-bearing Utf8
    // entries (populated only when `from_java_cesu8` rejects lone surrogates).
    let mut wide_utf8: std::collections::HashMap<u16, Arc<[u16]>> =
        std::collections::HashMap::new();

    let mut i = 1u16;
    while i < count {
        let tag = buf.read_u8()?;
        // HotSpot `parse_constant_pool_entries`: the method-handle,
        // method-type and invokedynamic tags need version 51, the dynamic
        // constant 55 (`Class file version does not support constant tag N`);
        // `CONSTANT_Module` / `CONSTANT_Package` below 53 are unknown tags
        // (interpreter round i5 wave 110, lane L5; probe
        // `L5W110ClassFileParserMore`, rows `tag-*-old`).
        let since = match tag {
            15 | 16 | 18 => 51,
            17 => 55,
            19 | 20 => 53,
            _ => 0,
        };
        if major < since {
            let text = if since == 53 {
                format!("Unknown constant tag {tag}")
            } else {
                format!("Class file version does not support constant tag {tag}")
            };
            return Err(ClassReaderError::class_file_parser(
                text,
                ClassReaderError::InvalidConstantPoolTag { index: i, tag },
            ));
        }
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
                // HotSpot `UTF8::is_legal_utf8` refuses a NUL byte and any
                // byte with high nibble 0xF (a 4-byte UTF-8 sequence); no
                // modified UTF-8 holds either. `from_java_cesu8` takes its
                // standard-UTF-8 fast path first and accepted a 4-byte
                // sequence (interpreter round i5 wave 109 hand-back, lane L5;
                // probe `L5W109ClassFileParserTexts`, row `illegal-utf8`).
                if bytes.iter().all(|&b| b != 0 && b < 0x80) {
                    // Safety: all bytes are non-zero 7-bit ASCII, which is valid UTF-8.
                    let s = unsafe { std::str::from_utf8_unchecked(bytes) };
                    ConstantPoolEntry::Utf8(if s.len() <= 24 { cratonvm_types::intern_arc(s) } else { Arc::from(s) }.into())
                } else if bytes.iter().any(|&b| b == 0 || b >= 0xF0) {
                    return Err(ClassReaderError::class_file_parser(
                        "Illegal UTF8 string in constant pool",
                        ClassReaderError::InvalidCesu8String { index: i },
                    ));
                } else {
                match cesu8::from_java_cesu8(bytes) {
                    Ok(string) => {
                        ConstantPoolEntry::Utf8(if string.len() <= 24 { cratonvm_types::intern_arc(&string) } else { Arc::from(string.as_ref()) }.into())
                    }
                    Err(_) => {
                        // `from_java_cesu8` rejects lone surrogates. Recover that
                        // specific case (e.g. ANTLR `_serializedATN`) by decoding
                        // leniently into UTF-16 units; the units are stashed in
                        // `wide_utf8` so the string constant materialises into the
                        // correct `java.lang.String` char[], while the Utf8 entry
                        // itself holds a lossy string for name/descriptor readers.
                        // Only accept the recovery when the failure is genuinely
                        // surrogate-caused (a surrogate unit is present); any other
                        // malformed input keeps the original hard error.
                        let units = decode_java_mutf8_to_utf16(bytes)
                            .ok()
                            .filter(|u| u.iter().any(|&c| (0xD800..=0xDFFF).contains(&c)))
                            .ok_or_else(|| {
                                // HotSpot `verify_legal_utf8`'s text
                                // (interpreter round i5 wave 109, lane L5).
                                ClassReaderError::class_file_parser(
                                    "Illegal UTF8 string in constant pool",
                                    ClassReaderError::InvalidCesu8String { index: i },
                                )
                            })?;
                        let lossy = String::from_utf16_lossy(&units);
                        wide_utf8.insert(i, Arc::from(units.into_boxed_slice()));
                        ConstantPoolEntry::Utf8(if lossy.len() <= 24 { cratonvm_types::intern_arc(&lossy) } else { Arc::from(lossy.as_str()) }.into())
                    }
                }
                }
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
                // JVMS §4.4.5: a Long/Double occupies two slots, so it must
                // not start at the last valid index (count-1) — its second
                // slot would overflow past the pool. Reject untrusted input
                // that places one there instead of silently growing the pool.
                if i + 1 >= count {
                    return Err(last_slot_category2(i, "CONSTANT_Long"));
                }
                let value = buf.read_i64()?;
                entries.push(ConstantPoolEntry::Long(value.into()));
                entries.push(ConstantPoolEntry::Tombstone);
                i += 2;
                continue;
            }
            6 => {
                // CONSTANT_Double (takes 2 slots) — see CONSTANT_Long above.
                if i + 1 >= count {
                    return Err(last_slot_category2(i, "CONSTANT_Double"));
                }
                let value = buf.read_f64()?;
                entries.push(ConstantPoolEntry::Double(value.into()));
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
                // HotSpot `parse_constant_pool_entries`' text (interpreter
                // round i5 wave 109, lane L5).
                return Err(ClassReaderError::class_file_parser(
                    format!("Unknown constant tag {tag}"),
                    ClassReaderError::InvalidConstantPoolTag { index: i, tag },
                ));
            }
        };

        entries.push(entry);
        i += 1;
    }

    // The 0-th sentinel plus `count - 1` real entries must total exactly
    // `count`. A Long/Double straddling the end would have grown the Vec
    // past `count`; that case is now rejected above, but verify the
    // invariant defensively before handing the pool to the rest of the VM.
    if entries.len() != count as usize {
        return Err(ClassReaderError::InvalidConstantPool {
            index: count,
            message: format!(
                "constant pool entry count mismatch: expected {count}, got {}",
                entries.len()
            ),
        });
    }
    check_constant_pool_symbol_refs(&entries)?;

    Ok(ConstantPool::new_with_wide(entries, wide_utf8))
}

fn read_field(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
    source: &SharedBytes,
    major: u16,
) -> Result<ClassFileField, ClassReaderError> {
    let access_flags_raw = buf.read_u16()?;
    let access_flags = FieldAccessFlags::from_bits_retain(access_flags_raw);
    let name_index = buf.read_u16()?;
    // Fetch the `Arc<str>` straight from the constant pool — it was already
    // interned at parse time via `cratonvm_types::intern_arc`, so this is a
    // single refcount bump (no allocation, no UTF-8 re-copy).
    // HotSpot `parse_fields`' texts (interpreter round i5 wave 109, lane L5).
    let name = constant_pool.get_utf8_arc(name_index).ok_or_else(|| {
        ClassReaderError::class_file_parser(
            format!("Invalid constant pool index {name_index} for field name"),
            ClassReaderError::InvalidConstantPool {
                index: name_index,
                message: "field name must reference a valid Utf8 entry".to_string(),
            },
        )
    })?;
    let descriptor_index = buf.read_u16()?;
    let descriptor = constant_pool
        .get_utf8_arc(descriptor_index)
        .ok_or_else(|| {
            ClassReaderError::class_file_parser(
                format!("Invalid constant pool index {descriptor_index} for field signature"),
                ClassReaderError::InvalidConstantPool {
                    index: descriptor_index,
                    message: "field descriptor must reference a valid Utf8 entry".to_string(),
                },
            )
        })?;
    let owner = if access_flags_raw & 0x0008 != 0 {
        AttributeOwner::StaticField
    } else {
        AttributeOwner::Field
    };
    let attributes = read_attributes(buf, constant_pool, source, owner, major)?;
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
    source: &SharedBytes,
    major: u16,
) -> Result<ClassFileMethod, ClassReaderError> {
    let access_flags_raw = buf.read_u16()?;
    let access_flags = MethodAccessFlags::from_bits_retain(access_flags_raw);
    let name_index = buf.read_u16()?;
    // Fetch the `Arc<str>` straight from the constant pool — it was already
    // interned at parse time via `cratonvm_types::intern_arc`, so this is a
    // single refcount bump (no allocation, no UTF-8 re-copy).
    // HotSpot `parse_method`'s texts (interpreter round i5 wave 109, lane L5):
    // "Illegal", not the fields' "Invalid".
    let name = constant_pool.get_utf8_arc(name_index).ok_or_else(|| {
        ClassReaderError::class_file_parser(
            format!("Illegal constant pool index {name_index} for method name"),
            ClassReaderError::InvalidConstantPool {
                index: name_index,
                message: "method name must reference a valid Utf8 entry".to_string(),
            },
        )
    })?;
    let descriptor_index = buf.read_u16()?;
    let descriptor = constant_pool
        .get_utf8_arc(descriptor_index)
        .ok_or_else(|| {
            ClassReaderError::class_file_parser(
                format!("Illegal constant pool index {descriptor_index} for method signature"),
                ClassReaderError::InvalidConstantPool {
                    index: descriptor_index,
                    message: "method descriptor must reference a valid Utf8 entry".to_string(),
                },
            )
        })?;
    let attributes = read_attributes(buf, constant_pool, source, AttributeOwner::Method, major)?;
    trace!("  Method: {name}{descriptor}");

    Ok(ClassFileMethod {
        access_flags,
        name,
        descriptor,
        attributes,
    })
}

/// Read an attribute table into a `Vec<LazyAttribute>`.
///
/// For every entry we read only the header (`attribute_name_index`,
/// `attribute_length`) and the raw body bytes — *no* structural parsing
/// happens here. The body is wrapped in a [`LazyAttribute::Raw`] that
/// downstream consumers decode on demand via
/// [`LazyAttribute::decode`] (which dispatches to
/// [`decode_attribute`]).
///
/// Two correctness checks survive the move to lazy parsing:
///
/// 1. **Buffer-bound check before slicing**: `attribute_length` must not
///    exceed the remaining buffer. This catches truncated class files
///    immediately rather than at decode time.
/// 2. **Per-attribute length verification**: by consuming exactly
///    `attribute_length` bytes via `buf.read_bytes(length)` we guarantee
///    every subsequent attribute starts at the declared offset. The
///    *content* validation (sub-parser must consume exactly the body)
///    moves into `decode_attribute` itself (see `attribute.rs`); both
///    paths therefore preserve the misalignment-prevention guarantee
///    that the eager reader had.
fn read_attributes(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
    source: &SharedBytes,
    owner: AttributeOwner,
    major: u16,
) -> Result<Vec<LazyAttribute>, ClassReaderError> {
    let count = buf.read_u16()?;
    validate_count("attributes", count, MAX_ATTRIBUTE_COUNT)?;
    // An attribute costs at least its 6-byte header, so a declared count
    // needing more bytes than remain is a truncated/hostile file. Rejecting
    // here means the reservation below is bounded by the input size.
    ensure_count_fits(
        "attributes",
        count as usize,
        ATTRIBUTE_HEADER_BYTES,
        buf.remaining(),
    )
    .map_err(truncated)?;
    let mut attributes = Vec::with_capacity(bounded_capacity(
        count as usize,
        ATTRIBUTE_HEADER_BYTES,
        buf.remaining(),
    ));

    for _ in 0..count {
        let name_index = buf.read_u16()?;
        // Fetch the interned `Arc<str>` directly — constant-pool Utf8 entries
        // were interned at parse time, so cloning the Arc is a refcount bump.
        let name = constant_pool.get_utf8_arc(name_index).ok_or_else(|| {
            ClassReaderError::class_file_parser(
                owner.bad_name_index_text(name_index),
                ClassReaderError::InvalidConstantPool {
                    index: name_index,
                    message: "attribute name must reference a valid Utf8 entry".to_string(),
                },
            )
        })?;
        // `attribute_length` is a u4 on the wire. `wire_len_to_usize` states
        // the narrowing explicitly rather than relying on `u32 as usize`
        // being lossless on every host the VM is built for.
        let length = wire_len_to_usize("attribute_length", buf.read_u32()?)?;

        // Validate attribute length does not exceed remaining buffer before
        // we slice into it. Catches truncated class files (and a hostile
        // `attribute_length` larger than the file) at read time.
        if length > buf.remaining() {
            return Err(truncated(ClassReaderError::InvalidClassData {
                message: format!(
                    "attribute '{name}' length {length} exceeds remaining buffer size {}",
                    buf.remaining()
                ),
            }));
        }

        // Snapshot the buffer position *before* consuming the body bytes —
        // that position is the body's offset in `source` (the
        // `ClassFileBuffer` was constructed from `&source[..]`, so its
        // position is a direct index into `source`).
        let start = buf.position();
        // Advance past the body. We deliberately discard the returned
        // slice: it would borrow from the `&[u8]` view that the buffer
        // wraps (lifetime `'a`), but we want to store an `Arc`-backed
        // range that lives independently of that borrow. The advance
        // itself is what enforces the per-attribute length verification:
        // any subsequent attribute starts at the spec-mandated offset,
        // eliminating the misalignment attack that the eager path's
        // snapshot/check wrapper guarded against.
        let _ = buf.read_bytes(length)?;
        // `read_bytes` already proved `start + length` is inside the buffer,
        // but compute the end with `checked_add` anyway: a wrapped `end`
        // would produce a *reversed* range that later slicing would panic
        // on rather than reject.
        let end = checked_end("attribute body range", start, length)?;

        // Eager shape-only validation for a small, fixed set of
        // attribute kinds whose laziness would otherwise hide
        // structural errors (fixed-size cp-index attributes + `Code`
        // body shape) until first downstream access. The full per-kind
        // decode remains lazy via `LazyAttribute::decode`; this walk
        // only runs the cheap structural predicate. See
        // `attribute::validate_attribute_shape` and the module-level
        // "Validation policy" docs in `attribute.rs` for the list and
        // the rationale.
        if !hotspot_reads_attribute(owner, &name, major) {
            // Skipped by HotSpot: kept lazily, a body that does not decode
            // becomes an opaque `Attribute::Unknown` (wave 111, L5).
            attributes.push(LazyAttribute::new_raw_lenient_in(name, source.clone(), start..end));
            continue;
        }
        // HotSpot's `Synthetic` / `Deprecated` length refusals, one text per
        // table, in every loader (wave 112, L5).
        if matches!(&*name, "Synthetic" | "Deprecated") && length != 0 {
            let table = match owner {
                AttributeOwner::Field | AttributeOwner::StaticField => "field",
                AttributeOwner::Method => "method",
                AttributeOwner::Class | AttributeOwner::RecordClass | AttributeOwner::ModuleInfo => {
                    "classfile"
                }
            };
            return Err(ClassReaderError::class_file_parser(
                format!("Invalid {name} {table} attribute length {length}"),
                ClassReaderError::InvalidClassData {
                    message: format!("{name} attribute must have attribute_length=0, got {length}"),
                },
            ));
        }
        // HotSpot's `Signature` length refusals, one text per table
        // (`parse_classfile_attributes`, `parse_field_attributes`,
        // `parse_method`; interpreter round i5 wave 111, lane L5).
        if &*name == "Signature" && length != 2 {
            let text = match owner {
                AttributeOwner::Class | AttributeOwner::RecordClass | AttributeOwner::ModuleInfo => {
                    format!("Wrong Signature attribute length {length}")
                }
                AttributeOwner::Field | AttributeOwner::StaticField => {
                    format!("Wrong size {length} for field's Signature attribute")
                }
                AttributeOwner::Method => format!("Invalid Signature attribute length {length}"),
            };
            return Err(ClassReaderError::class_file_parser(
                text,
                ClassReaderError::InvalidClassData {
                    message: format!("Signature attribute must have attribute_length=2, got {length}"),
                },
            ));
        }
        // HotSpot `parse_classfile_record_attribute` reads the components
        // from the class-file STREAM, not from the attribute's own bytes, so a
        // component table that runs past the end of the whole file is
        // `Truncated class file` before the length check can say "wrong
        // length" (measured on w112, `L5W112ClassFileParserRecord`
        // `record-not-record-super`).
        if &*name == "Record" && record_components_run_past_the_file(&source[start..]) {
            return Err(ClassReaderError::UnexpectedEndOfData { position: source.len() });
        }
        validate_attribute_shape(&name, &source[start..end])?;

        // Zero-copy construction: refcount-bump `source` and remember the
        // range. No per-attribute `Vec<u8>` allocation, no memcpy of the
        // body. On java.base bootstrap this saves ~25 MB of malloc churn
        // (~5 k classes × ~10 attrs × ~50 B average).
        attributes.push(LazyAttribute::new_raw_in(name, source.clone(), start..end));
    }

    Ok(attributes)
}

/// Whether HotSpot's stream read of a `Record` attribute's components
/// (`components_count`, then per component its name, descriptor,
/// `attributes_count` and each attribute's header and body) needs more bytes
/// than the class file has from the attribute's body on. `rest` is the file
/// from that body to its end.
fn record_components_run_past_the_file(rest: &[u8]) -> bool {
    let u2_at = |at: usize| {
        rest.get(at..at + 2)
            .map(|c| usize::from(u16::from_be_bytes([c[0], c[1]])))
    };
    let fits = (|| -> Option<()> {
        let count = u2_at(0)?;
        let mut at = 2usize;
        for _ in 0..count {
            let attributes = u2_at(at + 4)?;
            at += 6;
            for _ in 0..attributes {
                let len = rest.get(at + 2..at + 6)?;
                let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]);
                at = at.checked_add(6)?.checked_add(usize::try_from(len).ok()?)?;
                if at > rest.len() {
                    return None;
                }
            }
        }
        Some(())
    })();
    fits.is_none()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
