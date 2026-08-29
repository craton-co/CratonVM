// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class file reader — parses raw bytes into a [`ClassFile`] structure.
//!
//! Reference: <https://docs.oracle.com/javase/specs/jvms/se21/html/jvms-4.html>

use crate::attribute::*;
use crate::buffer::ClassFileBuffer;
use crate::byte_view::SharedBytes;
use crate::class_access_flags::*;
use crate::class_file::ClassFile;
use crate::class_file_version::{preview_enabled, ClassFileVersion};
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
pub fn read_class_shared(source: SharedBytes) -> Result<ClassFile, ClassReaderError> {
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
    // Preview gating (JVMS 4.1). The enablement bit is read here rather than
    // threaded through every entry point because HotSpot treats it the same
    // way: a whole-process property fixed before the first class is parsed.
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
    if let Err(rejection) = version.verify(preview_enabled()) {
        return Err(ClassReaderError::UnsupportedVersion {
            major,
            minor,
            rejection,
        });
    }
    debug!("Class file version: {version}");

    // Constant pool
    let constant_pool = read_constant_pool(&mut buf)?;
    debug!("Constant pool: {} entries", constant_pool.len());

    // Access flags, this class, super class
    let access_flags_raw = buf.read_u16()?;
    let access_flags = ClassAccessFlags::from_bits_retain(access_flags_raw);

    // Resolve `this_class` / `super_class` / interface names directly to
    // the pool-interned `Arc<str>` rather than allocating fresh Strings.
    // The constant pool already holds the canonical interned form of every
    // Utf8 entry, so each name becomes a single refcount bump on the
    // shared backing allocation. On bootstrap (~5 k classes, ~12 names
    // per class on average) this eliminates ~60 k throwaway String allocs.
    let this_class_index = buf.read_u16()?;
    let this_class = constant_pool
        .get_class_name_arc(this_class_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: this_class_index,
            message: "this_class must reference a valid Class entry".to_string(),
        })?;
    debug!("Parsing class: {this_class}");

    let super_class_index = buf.read_u16()?;
    let super_class = if super_class_index == 0 {
        None // java.lang.Object has no superclass
    } else {
        Some(
            constant_pool
                .get_class_name_arc(super_class_index)
                .ok_or_else(|| ClassReaderError::InvalidConstantPool {
                    index: super_class_index,
                    message: "super_class must reference a valid Class entry".to_string(),
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
    )?;
    let mut interfaces: Vec<Arc<str>> = Vec::with_capacity(bounded_capacity(
        interfaces_count as usize,
        INTERFACE_ENTRY_BYTES,
        buf.remaining(),
    ));
    for _ in 0..interfaces_count {
        let iface_index = buf.read_u16()?;
        let iface_name = constant_pool
            .get_class_name_arc(iface_index)
            .ok_or_else(|| ClassReaderError::InvalidConstantPool {
                index: iface_index,
                message: "interface must reference a valid Class entry".to_string(),
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
    )?;
    let mut fields = Vec::with_capacity(bounded_capacity(
        fields_count as usize,
        FIELD_ENTRY_BYTES,
        buf.remaining(),
    ));
    for _ in 0..fields_count {
        fields.push(read_field(&mut buf, &constant_pool, &source)?);
    }

    // Methods
    let methods_count = buf.read_u16()?;
    validate_count("methods", methods_count, MAX_METHOD_COUNT)?;
    ensure_count_fits(
        "methods",
        methods_count as usize,
        METHOD_ENTRY_BYTES,
        buf.remaining(),
    )?;
    let mut methods = Vec::with_capacity(bounded_capacity(
        methods_count as usize,
        METHOD_ENTRY_BYTES,
        buf.remaining(),
    ));
    for _ in 0..methods_count {
        methods.push(read_method(&mut buf, &constant_pool, &source)?);
    }

    // Class attributes
    let attributes = read_attributes(&mut buf, &constant_pool, &source)?;
    if buf.remaining() != 0 {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "class file has {} trailing bytes after class attributes",
                buf.remaining()
            ),
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

fn read_constant_pool(buf: &mut ClassFileBuffer) -> Result<ConstantPool, ClassReaderError> {
    let count = buf.read_u16()?;
    // JVMS §4.1: constant_pool_count must be >= 1. The 0-th slot is a
    // reserved sentinel; valid entries live at indices 1..count. HotSpot
    // rejects count == 0 with java.lang.ClassFormatError.
    if count == 0 {
        return Err(ClassReaderError::InvalidConstantPool {
            index: 0,
            message: "constant_pool_count must be >= 1".to_string(),
        });
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
    )?;
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
                match cesu8::from_java_cesu8(bytes) {
                    Ok(string) => ConstantPoolEntry::Utf8(cratonvm_types::intern_arc(&string)),
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
                            .ok_or(ClassReaderError::InvalidCesu8String { index: i })?;
                        let lossy = String::from_utf16_lossy(&units);
                        wide_utf8.insert(i, Arc::from(units.into_boxed_slice()));
                        ConstantPoolEntry::Utf8(cratonvm_types::intern_arc(&lossy))
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
                    return Err(ClassReaderError::InvalidConstantPool {
                        index: i,
                        message: "CONSTANT_Long must not occupy the last constant pool slot"
                            .to_string(),
                    });
                }
                let value = buf.read_i64()?;
                entries.push(ConstantPoolEntry::Long(value));
                entries.push(ConstantPoolEntry::Tombstone);
                i += 2;
                continue;
            }
            6 => {
                // CONSTANT_Double (takes 2 slots) — see CONSTANT_Long above.
                if i + 1 >= count {
                    return Err(ClassReaderError::InvalidConstantPool {
                        index: i,
                        message: "CONSTANT_Double must not occupy the last constant pool slot"
                            .to_string(),
                    });
                }
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

    Ok(ConstantPool::new_with_wide(entries, wide_utf8))
}

fn read_field(
    buf: &mut ClassFileBuffer,
    constant_pool: &ConstantPool,
    source: &SharedBytes,
) -> Result<ClassFileField, ClassReaderError> {
    let access_flags_raw = buf.read_u16()?;
    let access_flags = FieldAccessFlags::from_bits_retain(access_flags_raw);
    let name_index = buf.read_u16()?;
    // Fetch the `Arc<str>` straight from the constant pool — it was already
    // interned at parse time via `cratonvm_types::intern_arc`, so this is a
    // single refcount bump (no allocation, no UTF-8 re-copy).
    let name = constant_pool.get_utf8_arc(name_index).ok_or_else(|| {
        ClassReaderError::InvalidConstantPool {
            index: name_index,
            message: "field name must reference a valid Utf8 entry".to_string(),
        }
    })?;
    let descriptor_index = buf.read_u16()?;
    let descriptor = constant_pool
        .get_utf8_arc(descriptor_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: descriptor_index,
            message: "field descriptor must reference a valid Utf8 entry".to_string(),
        })?;
    let attributes = read_attributes(buf, constant_pool, source)?;
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
) -> Result<ClassFileMethod, ClassReaderError> {
    let access_flags_raw = buf.read_u16()?;
    let access_flags = MethodAccessFlags::from_bits_retain(access_flags_raw);
    let name_index = buf.read_u16()?;
    // Fetch the `Arc<str>` straight from the constant pool — it was already
    // interned at parse time via `cratonvm_types::intern_arc`, so this is a
    // single refcount bump (no allocation, no UTF-8 re-copy).
    let name = constant_pool.get_utf8_arc(name_index).ok_or_else(|| {
        ClassReaderError::InvalidConstantPool {
            index: name_index,
            message: "method name must reference a valid Utf8 entry".to_string(),
        }
    })?;
    let descriptor_index = buf.read_u16()?;
    let descriptor = constant_pool
        .get_utf8_arc(descriptor_index)
        .ok_or_else(|| ClassReaderError::InvalidConstantPool {
            index: descriptor_index,
            message: "method descriptor must reference a valid Utf8 entry".to_string(),
        })?;
    let attributes = read_attributes(buf, constant_pool, source)?;
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
    )?;
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
            ClassReaderError::InvalidConstantPool {
                index: name_index,
                message: "attribute name must reference a valid Utf8 entry".to_string(),
            }
        })?;
        // `attribute_length` is a u4 on the wire. `wire_len_to_usize` states
        // the narrowing explicitly rather than relying on `u32 as usize`
        // being lossless on every host the VM is built for.
        let length = wire_len_to_usize("attribute_length", buf.read_u32()?)?;

        // Validate attribute length does not exceed remaining buffer before
        // we slice into it. Catches truncated class files (and a hostile
        // `attribute_length` larger than the file) at read time.
        if length > buf.remaining() {
            return Err(ClassReaderError::InvalidClassData {
                message: format!(
                    "attribute '{name}' length {length} exceeds remaining buffer size {}",
                    buf.remaining()
                ),
            });
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
        validate_attribute_shape(&name, &source[start..end])?;

        // Zero-copy construction: refcount-bump `source` and remember the
        // range. No per-attribute `Vec<u8>` allocation, no memcpy of the
        // body. On java.base bootstrap this saves ~25 MB of malloc churn
        // (~5 k classes × ~10 attrs × ~50 B average).
        attributes.push(LazyAttribute::new_raw_in(name, source.clone(), start..end));
    }

    Ok(attributes)
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

    // ── Lone-surrogate modified-UTF-8 decode (SB-13) ─────────────────────

    /// Encode a single UTF-16 code unit as a 3-byte modified-UTF-8 sequence
    /// (the form HotSpot uses for surrogates and for U+0800..U+FFFF).
    fn mutf8_3byte(u: u16) -> [u8; 3] {
        [
            0xE0 | ((u >> 12) as u8 & 0x0F),
            0x80 | ((u >> 6) as u8 & 0x3F),
            0x80 | (u as u8 & 0x3F),
        ]
    }

    #[test]
    fn decode_mutf8_lone_surrogate_roundtrips() {
        // "A" + lone high surrogate U+D834 + "B"
        let mut bytes = vec![b'A'];
        bytes.extend_from_slice(&mutf8_3byte(0xD834));
        bytes.push(b'B');
        let units = decode_java_mutf8_to_utf16(&bytes).expect("decode ok");
        assert_eq!(units, vec![0x41, 0xD834, 0x42]);
        // cesu8 must reject this exact input — that is what triggers the path.
        assert!(cesu8::from_java_cesu8(&bytes).is_err());
    }

    #[test]
    fn decode_mutf8_surrogate_pair_becomes_two_units() {
        // U+1D11E (G clef) = surrogate pair D834 DD1E in UTF-16.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&mutf8_3byte(0xD834));
        bytes.extend_from_slice(&mutf8_3byte(0xDD1E));
        let units = decode_java_mutf8_to_utf16(&bytes).expect("decode ok");
        assert_eq!(units, vec![0xD834, 0xDD1E]);
    }

    #[test]
    fn decode_mutf8_nul_and_two_byte() {
        // Modified UTF-8 NUL (0xC0 0x80) and a 2-byte U+00E9.
        let bytes = [0xC0, 0x80, 0xC3, 0xA9];
        let units = decode_java_mutf8_to_utf16(&bytes).expect("decode ok");
        assert_eq!(units, vec![0x0000, 0x00E9]);
    }

    #[test]
    fn decode_mutf8_rejects_malformed() {
        assert!(decode_java_mutf8_to_utf16(&[0x00]).is_err()); // bare NUL
        assert!(decode_java_mutf8_to_utf16(&[0xE0, 0x80]).is_err()); // truncated 3-byte
        assert!(decode_java_mutf8_to_utf16(&[0xC0]).is_err()); // truncated 2-byte
        assert!(decode_java_mutf8_to_utf16(&[0xF0, 0x90, 0x80, 0x80]).is_err()); // 4-byte
        assert!(decode_java_mutf8_to_utf16(&[0xE0, 0x20, 0x80]).is_err()); // bad cont
    }

    #[test]
    fn constant_pool_carries_wide_units_for_surrogate_utf8() {
        // Build a minimal class CP: [0]=Tombstone, [1]=Utf8 with a lone surrogate.
        let mut data = Vec::new();
        push_u16(&mut data, 2); // constant_pool_count (1 real entry at index 1)
        data.push(1); // CONSTANT_Utf8 tag
        let mut payload = vec![b'x'];
        payload.extend_from_slice(&mutf8_3byte(0xDC00)); // lone low surrogate
        push_u16(&mut data, payload.len() as u16);
        data.extend_from_slice(&payload);

        let mut buf = ClassFileBuffer::new(&data);
        let cp = read_constant_pool(&mut buf).expect("cp parses despite surrogate");
        // The Utf8 entry exists (lossy form) and the side table has exact units.
        assert!(cp.get_utf8(1).is_some());
        assert_eq!(cp.get_utf8_wide(1), Some([b'x' as u16, 0xDC00].as_slice()));
    }

    // ── Attribute parsing tests ──────────────────────────────────────────
    //
    // The per-attribute body decoders (annotation parsing, type annotations,
    // Module, Code, …) live in `reader/src/attribute.rs` after T11 — see
    // the unit tests there for fine-grained coverage of each decoder. The
    // tests below exercise the *reader* side: `read_attributes` produces
    // `LazyAttribute::Raw`, which the consumer decodes on demand. Each test
    // calls `.decode(cp)` to verify the raw bytes round-trip through
    // `decode_attribute` correctly.

    /// Helper to build a minimal constant pool and parse a single attribute.
    /// Returns the decoded [`Attribute`] so existing assertions can match
    /// directly on attribute variants.
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

        let source = SharedBytes::from(full.as_slice());
        let mut buf = ClassFileBuffer::new(&source);
        let mut attrs = read_attributes(&mut buf, &cp, &source).unwrap();
        assert_eq!(attrs.len(), 1);
        // Decode the single lazy attribute and return an owned `Attribute`.
        // We `decode` then clone out so the caller gets a value (the lazy
        // attribute is dropped at the end of this function).
        attrs[0]
            .decode(&cp)
            .expect("attribute body must decode")
            .clone()
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

    #[test]
    fn read_attributes_returns_lazy_raw_by_default() {
        // Verify that `read_attributes` produces `LazyAttribute::Raw`
        // entries without decoding them — the whole point of T11.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("ModulePackages".into()),
        ]);
        let mut body = Vec::new();
        push_u16(&mut body, 0); // packages count = 0

        let mut raw = Vec::new();
        push_u16(&mut raw, 1); // attributes_count
        push_u16(&mut raw, 1); // name index
        let len = body.len() as u32;
        raw.extend_from_slice(&len.to_be_bytes());
        raw.extend_from_slice(&body);

        let source = SharedBytes::from(raw.as_slice());
        let mut buf = ClassFileBuffer::new(&source);
        let attrs = read_attributes(&mut buf, &cp, &source).unwrap();
        assert_eq!(attrs.len(), 1);
        assert!(
            !attrs[0].is_decoded(),
            "attributes must come back lazy, not pre-decoded"
        );
        assert_eq!(attrs[0].name(), "ModulePackages");
    }

    #[test]
    fn read_attributes_rejects_length_exceeding_buffer() {
        // attribute_length larger than remaining buffer must fail at read
        // time, not at decode time — preserving the truncation guard from
        // the eager reader.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("ModulePackages".into()),
        ]);
        let mut raw = Vec::new();
        push_u16(&mut raw, 1); // attributes_count
        push_u16(&mut raw, 1); // name index
                               // Declared length = 1_000_000, but no body bytes follow.
        raw.extend_from_slice(&1_000_000_u32.to_be_bytes());
        let source = SharedBytes::from(raw.as_slice());
        let mut buf = ClassFileBuffer::new(&source);
        assert!(read_attributes(&mut buf, &cp, &source).is_err());
    }

    // ── Resource limits / safety tests ───────────────────────────────────

    #[test]
    fn prealloc_cap_constant_exists_and_is_bounded() {
        // Verify the PREALLOC_CAP constant is set to a reasonable value
        // to prevent excessive pre-allocation on malformed class files.
        // The constant now lives in `crate::limits` so every parser in the
        // crate shares one definition.
        let cap = crate::limits::PREALLOC_CAP;
        assert!(cap > 0, "PREALLOC_CAP must be positive");
        assert!(
            cap <= 65536,
            "PREALLOC_CAP should be bounded to prevent excessive allocation"
        );
    }

    // ── Declared-count vs. remaining-input boundary tests ────────────────
    //
    // Each "must reject" case below has a "must accept" twin built from the
    // same helper with a count the input can actually hold, so the suite
    // cannot pass by rejecting everything.

    /// Build a class-file prefix up to (and including) `constant_pool_count`.
    fn class_prefix_with_cp_count(count: u16) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0xCAFEBABE_u32.to_be_bytes());
        data.extend_from_slice(&0_u16.to_be_bytes()); // minor
        data.extend_from_slice(&52_u16.to_be_bytes()); // major (Java 8)
        data.extend_from_slice(&count.to_be_bytes());
        data
    }

    #[test]
    fn constant_pool_count_beyond_remaining_bytes_is_rejected_before_allocating() {
        // A 10-byte file that claims 65 535 constant-pool slots. Without
        // the `ensure_count_fits` guard this reserves ~1.5 MB and only
        // then fails on the first tag read.
        let data = class_prefix_with_cp_count(u16::MAX);
        let err = read_class(&data).expect_err("hostile cp_count must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("constant_pool") && msg.contains("bytes"),
            "expected a count-vs-remaining rejection, got: {msg}"
        );
    }

    #[test]
    fn constant_pool_count_within_remaining_bytes_is_not_rejected_by_the_size_guard() {
        // Must-accept twin: the same guard must not fire on a pool whose
        // declared slots fit in the bytes that follow. This file is still
        // truncated (no access_flags/this_class), so it errors — but it
        // must error *past* the size guard, i.e. not with that message.
        let mut data = class_prefix_with_cp_count(2); // one real entry
        data.push(1); // CONSTANT_Utf8
        data.extend_from_slice(&4_u16.to_be_bytes());
        data.extend_from_slice(b"Test");
        let err = read_class(&data).expect_err("file is truncated after the pool");
        let msg = err.to_string();
        assert!(
            !msg.contains("declared count"),
            "the size guard must not fire on a pool that fits: {msg}"
        );
    }

    #[test]
    fn constant_pool_count_of_one_is_the_empty_pool_boundary() {
        // count == 1 means "sentinel only, no entries" — zero-length
        // boundary. The guard must not reject it even with no bytes left
        // for entries (the file is still truncated afterwards, but for a
        // different reason).
        let data = class_prefix_with_cp_count(1);
        let err = read_class(&data).expect_err("file ends after the pool count");
        assert!(
            !err.to_string().contains("declared count"),
            "an empty constant pool must clear the size guard"
        );
        // And count == 0 is a JVMS §4.1 violation, rejected separately.
        let zero = class_prefix_with_cp_count(0);
        assert!(read_class(&zero).is_err());
    }

    #[test]
    fn section_count_guards_reject_impossible_declarations() {
        // Drive `ensure_count_fits` at exactly the boundary each top-level
        // section uses, both directions. This is the unit-level twin of
        // the whole-file tests above.
        for (label, entry_bytes) in [
            ("interfaces", INTERFACE_ENTRY_BYTES),
            ("fields", FIELD_ENTRY_BYTES),
            ("methods", METHOD_ENTRY_BYTES),
            ("attributes", ATTRIBUTE_HEADER_BYTES),
        ] {
            // u16::MAX entries can never fit in a 40-byte tail.
            assert!(
                ensure_count_fits(label, u16::MAX as usize, entry_bytes, 40).is_err(),
                "{label}: 65535 entries must not fit in 40 bytes"
            );
            // Exactly as many as fit is accepted; one more is not.
            let fits = 40 / entry_bytes;
            assert!(ensure_count_fits(label, fits, entry_bytes, 40).is_ok());
            assert!(ensure_count_fits(label, fits + 1, entry_bytes, 40).is_err());
            // Zero entries always fit, even in an empty tail.
            assert!(ensure_count_fits(label, 0, entry_bytes, 0).is_ok());
        }
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
        // matching string returned by `cratonvm_types::intern_arc`.
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
        // `cratonvm_types::intern_arc` at parse time.
        for arc in &utf8_arcs {
            let pooled = cratonvm_types::intern_arc(arc);
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
