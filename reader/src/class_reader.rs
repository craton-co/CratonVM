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
use std::sync::Arc;
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
// MAX_EXCEPTION_TABLE_COUNT was used by the eager `Code` parser; that parser
// now lives in `attribute.rs` (called on-demand via `LazyAttribute::decode`),
// so the constant is no longer needed here.

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
    // Wrap the input bytes in a single shared `Arc<[u8]>` that will be
    // threaded through every `LazyAttribute::Raw` produced for this class
    // file. Each lazy attribute is then a `(name: Arc<str>, source:
    // Arc<[u8]>, range: Range<usize>)` — a refcount bump on the source plus
    // a range, no body memcpy. The Arc is kept alive by whichever lazy
    // attribute(s) survive parsing; once they decode or drop, the backing
    // buffer is freed.
    //
    // The single `Arc::from(data)` here copies `data` once into a fresh
    // heap allocation. That's one O(class_file_size) copy at parse entry,
    // replacing potentially hundreds of per-attribute `to_vec()` copies
    // inside `read_attributes`.
    let source: Arc<[u8]> = Arc::from(data);

    let mut buf = ClassFileBuffer::new(&source);

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
        Some(constant_pool.get_class_name_arc(super_class_index).ok_or_else(
            || ClassReaderError::InvalidConstantPool {
                index: super_class_index,
                message: "super_class must reference a valid Class entry".to_string(),
            },
        )?)
    };

    // Interfaces
    let interfaces_count = buf.read_u16()?;
    validate_count("interfaces", interfaces_count, MAX_INTERFACE_COUNT)?;
    let mut interfaces: Vec<Arc<str>> =
        Vec::with_capacity((interfaces_count as usize).min(PREALLOC_CAP));
    for _ in 0..interfaces_count {
        let iface_index = buf.read_u16()?;
        let iface_name = constant_pool.get_class_name_arc(iface_index).ok_or_else(|| {
            ClassReaderError::InvalidConstantPool {
                index: iface_index,
                message: "interface must reference a valid Class entry".to_string(),
            }
        })?;
        interfaces.push(iface_name);
    }

    // Fields
    let fields_count = buf.read_u16()?;
    validate_count("fields", fields_count, MAX_FIELD_COUNT)?;
    let mut fields = Vec::with_capacity((fields_count as usize).min(PREALLOC_CAP));
    for _ in 0..fields_count {
        fields.push(read_field(&mut buf, &constant_pool, &source)?);
    }

    // Methods
    let methods_count = buf.read_u16()?;
    validate_count("methods", methods_count, MAX_METHOD_COUNT)?;
    let mut methods = Vec::with_capacity((methods_count as usize).min(PREALLOC_CAP));
    for _ in 0..methods_count {
        methods.push(read_method(&mut buf, &constant_pool, &source)?);
    }

    // Class attributes
    let attributes = read_attributes(&mut buf, &constant_pool, &source)?;

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
    source: &Arc<[u8]>,
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
    source: &Arc<[u8]>,
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
    source: &Arc<[u8]>,
) -> Result<Vec<LazyAttribute>, ClassReaderError> {
    let count = buf.read_u16()?;
    validate_count("attributes", count, MAX_ATTRIBUTE_COUNT)?;
    let mut attributes = Vec::with_capacity((count as usize).min(PREALLOC_CAP));

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
        let length = buf.read_u32()? as usize;

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
        let end = start + length;

        // Zero-copy construction: refcount-bump `source` and remember the
        // range. No per-attribute `Vec<u8>` allocation, no memcpy of the
        // body. On java.base bootstrap this saves ~25 MB of malloc churn
        // (~5 k classes × ~10 attrs × ~50 B average).
        attributes.push(LazyAttribute::new_raw_in(
            name,
            Arc::clone(source),
            start..end,
        ));
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

        let source: Arc<[u8]> = Arc::from(full.as_slice());
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

        let source: Arc<[u8]> = Arc::from(raw.as_slice());
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
        let source: Arc<[u8]> = Arc::from(raw.as_slice());
        let mut buf = ClassFileBuffer::new(&source);
        assert!(read_attributes(&mut buf, &cp, &source).is_err());
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
