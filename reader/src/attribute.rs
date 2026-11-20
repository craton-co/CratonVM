// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Attribute decoding (JVM spec §4.7) and the lazy-decode container.
//!
//! # Validation policy (HIGH, 2026-05-24)
//!
//! The class-reader hot path wraps every attribute body as a
//! [`LazyAttribute::Raw`] and *only* decodes it on first downstream
//! access. That preserves the lazy-decode performance win for the ~70 %
//! of attributes (LocalVariableTable, RuntimeInvisibleAnnotations,
//! Module, …) that bootstrap never consumes — but it means structural
//! errors that *should* be caught at class-load time (the moment the
//! reader hands a `ClassFile` back to the loader) get deferred until
//! some random downstream call site finally asks for the parsed form.
//!
//! For a small, well-defined set of attribute kinds whose **shape is
//! cheap to validate without producing the decoded value**, we run a
//! "validation-only walk" at `read_class` time via
//! [`validate_attribute_shape`] in [`class_reader::read_attributes`]:
//!
//! * `ConstantValue` — must be exactly 2 bytes (JVMS §4.7.2)
//! * `NestHost` — must be exactly 2 bytes (JVMS §4.7.28)
//! * `ModuleMainClass` — must be exactly 2 bytes (JVMS §4.7.27)
//! * `EnclosingMethod` — must be exactly 4 bytes (JVMS §4.7.7)
//! * `Code` — `code_length` must be in `1..=65535` (JVMS §4.7.3) and
//!   the body must hold a well-shaped exception table + nested
//!   attribute table within the declared `attribute_length`. Nested
//!   attributes are recursively shape-validated. Each `exception_table`
//!   entry's `start_pc` / `end_pc` / `handler_pc` is range-checked
//!   against `code_length` (see [`validate_exception_range`]); the
//!   entry's `catch_type` needs the constant pool, so it is checked in
//!   [`decode_attribute`] instead.
//!
//! These checks duplicate the logic that the lazy decoder runs later,
//! but they're tiny — a `u16`/`u32` read, a bounds comparison, no
//! string allocation, no constant-pool lookup — so the cost of the
//! eager walk is dwarfed by the cost of producing the
//! `LazyAttribute::Raw` itself.
//!
//! **Do not extend this list casually.** Every additional kind we
//! validate eagerly defeats the lazy-decode win for that kind. The
//! current list is exactly the set whose laziness was hiding
//! reachable parser-shape bugs in malformed-input regression tests; if
//! you add another, document the JVMS clause and the test that
//! motivated it.

use std::ops::Range;
use std::sync::Arc;

use crate::buffer::ClassFileBuffer;
use crate::byte_view::{ByteView, SharedBytes};
use crate::class_reader_error::ClassReaderError;
use crate::constant_pool::ConstantPool;
// Every resource limit this module enforces — preallocation cap, nesting
// depths, per-entry wire sizes — is defined once in `crate::limits`, along
// with the checked-arithmetic helpers. See `docs/security/reader/limits.md`.
use crate::limits::{
    bounded_capacity, checked_end, checked_span, wire_len_to_usize, EXCEPTIONS_ENTRY_SIZE,
    EXCEPTION_TABLE_ENTRY_SIZE, INNER_CLASS_ENTRY_SIZE, LINE_NUMBER_ENTRY_SIZE,
    LOCALVAR_TARGET_ENTRY_SIZE, LOCAL_VARIABLE_ENTRY_SIZE, METHOD_PARAMETER_ENTRY_SIZE,
    PREALLOC_CAP,
};

// ---------------------------------------------------------------------------
// Canonical attribute-name interned arcs for Arc::ptr_eq dispatch.
//
// Round 9 audit fix (MED #9): hoist the 28+2 LazyLock<Arc<str>>
// declarations out of `decode_attribute_body` so they're created exactly
// once per process (instead of being function-local statics that, while
// also initialized once, lived inside the body and bloated the
// function-scope namespace). Identical semantics — these are still
// `LazyLock<Arc<str>>` populated by `cratonvm_types::intern_arc(...)` and
// match the constant-pool-interned `Arc<str>` the lazy decode path
// hands us — but now the symbols are visible to the whole module and
// don't need re-declaring if we add another dispatch site.
//
// Round 9 audit fix (HIGH #4): the original `Arc::ptr_eq` dispatch
// omitted `ModulePackages` + `ModuleMainClass`, so module-info classes
// (`module-info.class` from JPMS, JDK 9+) paid the `&**name`
// string-compare slow path on every load. JDK ships hundreds of
// module-info classes; the canonical names below short-circuit the
// dispatch.
// ---------------------------------------------------------------------------
use std::sync::LazyLock;
macro_rules! canon {
    ($name:ident, $s:expr) => {
        static $name: LazyLock<Arc<str>> = LazyLock::new(|| cratonvm_types::intern_arc($s));
    };
}
canon!(CANON_CODE, "Code");
canon!(CANON_SOURCE_FILE, "SourceFile");
canon!(CANON_LINE_NUMBER_TABLE, "LineNumberTable");
canon!(CANON_LOCAL_VARIABLE_TABLE, "LocalVariableTable");
canon!(CANON_LOCAL_VARIABLE_TYPE_TABLE, "LocalVariableTypeTable");
canon!(CANON_STACK_MAP_TABLE, "StackMapTable");
canon!(CANON_CONSTANT_VALUE, "ConstantValue");
canon!(CANON_EXCEPTIONS, "Exceptions");
canon!(CANON_SIGNATURE, "Signature");
canon!(CANON_INNER_CLASSES, "InnerClasses");
canon!(CANON_BOOTSTRAP_METHODS, "BootstrapMethods");
canon!(CANON_DEPRECATED, "Deprecated");
canon!(CANON_SYNTHETIC, "Synthetic");
canon!(CANON_NEST_HOST, "NestHost");
canon!(CANON_NEST_MEMBERS, "NestMembers");
canon!(
    CANON_RUNTIME_VISIBLE_ANNOTATIONS,
    "RuntimeVisibleAnnotations"
);
canon!(
    CANON_RUNTIME_INVISIBLE_ANNOTATIONS,
    "RuntimeInvisibleAnnotations"
);
canon!(CANON_METHOD_PARAMETERS, "MethodParameters");
canon!(CANON_ENCLOSING_METHOD, "EnclosingMethod");
canon!(
    CANON_RUNTIME_VISIBLE_PARAMETER_ANNOTATIONS,
    "RuntimeVisibleParameterAnnotations"
);
canon!(
    CANON_RUNTIME_INVISIBLE_PARAMETER_ANNOTATIONS,
    "RuntimeInvisibleParameterAnnotations"
);
canon!(
    CANON_RUNTIME_VISIBLE_TYPE_ANNOTATIONS,
    "RuntimeVisibleTypeAnnotations"
);
canon!(
    CANON_RUNTIME_INVISIBLE_TYPE_ANNOTATIONS,
    "RuntimeInvisibleTypeAnnotations"
);
canon!(CANON_ANNOTATION_DEFAULT, "AnnotationDefault");
canon!(CANON_MODULE, "Module");
canon!(CANON_RECORD, "Record");
canon!(CANON_PERMITTED_SUBCLASSES, "PermittedSubclasses");
canon!(CANON_SOURCE_DEBUG_EXTENSION, "SourceDebugExtension");
// Round 9 audit fix (HIGH #4): module-info attributes — previously missing.
canon!(CANON_MODULE_PACKAGES, "ModulePackages");
canon!(CANON_MODULE_MAIN_CLASS, "ModuleMainClass");

/// Attributes attached to class files, fields, methods, and code (JVM spec 4.7).
///
/// Attributes provide additional metadata. Some are critical for execution (Code),
/// some for debugging (LineNumberTable), and some for reflection (Signature).
#[derive(Debug, Clone)]
pub enum Attribute {
    /// The `Code` attribute (4.7.3): contains the bytecode for a method.
    ///
    /// Boxed (zr z11/md, 2026-10-13): `CodeAttribute` is 96 bytes, and the
    /// widest variant sets the size of EVERY `Attribute` slot -- each method's
    /// `LineNumberTable`, `LocalVariableTable` and `StackMapTable` inside its
    /// `Code` included (~28 slots per JDK class). Boxed, an `Attribute` is 32
    /// bytes; see `ATTRIBUTE_SLOT_BYTES` and the census in
    /// `classloading/tests/zr8_fm_class_metadata_census.rs`.
    Code(Box<CodeAttribute>),

    /// The `SourceFile` attribute (4.7.10): the source file name.
    ///
    /// Stored as `Arc<str>` — the constant-pool Utf8 entry is already
    /// interned via `cratonvm_types::intern_arc`, so this is a refcount-bump
    /// clone of the pool's backing allocation (no per-class String alloc).
    SourceFile(Arc<str>),

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
    ///
    /// Stored as `Arc<str>` — same rationale as `SourceFile`. Refcount-bump
    /// clone of the pool-interned name; no fresh allocation per attribute.
    Signature(Arc<str>),

    /// The `StackMapTable` attribute (4.7.4): verification type info for each basic block.
    ///
    /// `entries` is stored as a [`ByteView`] — a zero-copy slice of the
    /// shared class-file `Arc<[u8]>`. Producing this attribute on the
    /// hot path is a single `Arc::clone` refcount bump plus two `usize`
    /// copies, no memcpy. The verifier reads it via deref coercion
    /// (`ByteView` → `&[u8]`).
    ///
    /// Boxed (zr z11/md): a `ByteView` is 40 bytes, wider than the 32-byte
    /// slot. Readers deref through the box unchanged (`&**entries` or deref
    /// coercion to `&[u8]`).
    StackMapTable { entries: Box<ByteView> },

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
    /// Boxed (zr z11/md): five vectors would make every slot 128 bytes.
    Module(Box<ModuleAttribute>),

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
    /// Boxed (zr z11/md): an `ElementValue` is wider than the 32-byte slot.
    AnnotationDefault(Box<ElementValue>),

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
    ///
    /// `name` is an `Arc<str>` (refcount-bump clone of the constant-pool
    /// interned attribute name) and `data` is a [`ByteView`] slicing
    /// directly into the shared class-file `Arc<[u8]>` — both avoid
    /// per-attribute allocations on the parse hot path. Most class
    /// files have several `Unknown`-tagged attributes (older annotation
    /// extensions, vendor attributes), so this is a measurable
    /// bootstrap win.
    ///
    /// `data` is boxed (zr z11/md) so the variant fits the 32-byte slot.
    Unknown { name: Arc<str>, data: Box<ByteView> },
}

/// The size of one [`Attribute`] slot (zr z11/md, 2026-10-13). Every variant
/// wider than a `Vec` is boxed so that this stays at 32 bytes on 64-bit
/// targets; `attribute_slot_is_32_bytes` pins it.
pub const ATTRIBUTE_SLOT_BYTES: usize = std::mem::size_of::<Attribute>();

/// The body of a `Module` attribute (JVM spec 4.7.25), boxed inside
/// [`Attribute::Module`].
#[derive(Debug, Clone)]
pub struct ModuleAttribute {
    pub name_index: u16,
    pub flags: u16,
    pub version_index: u16,
    pub requires: Vec<ModuleRequires>,
    pub exports: Vec<ModuleExports>,
    pub opens: Vec<ModuleOpens>,
    pub uses: Vec<u16>,
    pub provides: Vec<ModuleProvides>,
}

/// The Code attribute structure (JVM spec 4.7.3).
///
/// `code` is stored as a [`ByteView`] so the body bytes are *not*
/// memcpy'd out of the class file buffer on parse. The decoder builds
/// the view over the shared class-file `Arc<[u8]>`; the per-method
/// cost is one `Arc::clone` refcount bump plus two `usize` copies, not
/// a `Vec<u8>` allocation + `MemCopy(code_length)`. On java.base
/// bootstrap this saves on the order of one allocation + memcpy per
/// method × ~24 k methods. Consumers that previously took `&[u8]` work
/// unchanged thanks to deref coercion (`ByteView` → `&[u8]`); consumers
/// that need an owned `Arc<[u8]>` (e.g. `VtableMethodSnapshot.code`,
/// `Frame.code`) call [`ByteView::to_arc`] which materializes a fresh
/// standalone allocation exactly once.
#[derive(Debug, Clone)]
pub struct CodeAttribute {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: ByteView,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// What HotSpot's `ClassFileParser` makes of an `InnerClasses` table when it
/// verifies the class file (`_need_verify`: every loader but the boot loader).
/// Interpreter round i5 wave 101, lane L3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InnerClassesVerdict {
    /// The table is kept as it is.
    Keep,
    /// `check_inner_classes_circularity` found a circular outer chain, or
    /// two entries with the same `inner_class_info_index`: HotSpot IGNORES
    /// the whole attribute (no member, no declaring class; `EnclosingMethod`
    /// is kept).
    Ignore,
    /// An entry whose inner and outer indexes are equal: `ClassFormatError:
    /// Class is both outer and inner class in class file <name>`.
    BothOuterAndInner,
    /// Two entries equal in all four fields: `ClassFormatError: Duplicate
    /// entry in InnerClasses attribute in class file <name>`.
    DuplicateEntry,
}

/// [`InnerClassesVerdict`] of `entries`, in HotSpot's order: the per-entry
/// inner == outer check while parsing, then `check_inner_classes_circularity`
/// entry by entry (the outer-chain loop first, then each later entry: an exact
/// repeat refuses, a repeated inner index ignores the table).
pub fn inner_classes_verdict(entries: &[InnerClassInfo], cp: &ConstantPool) -> InnerClassesVerdict {
    // (Index 0 is left to the index checks HotSpot makes before this one.)
    if entries.iter().any(|e| {
        e.inner_class_info_index != 0 && e.inner_class_info_index == e.outer_class_info_index
    }) {
        return InnerClassesVerdict::BothOuterAndInner;
    }
    let name = |index: u16| {
        if index == 0 {
            None
        } else {
            cp.get_class_name(index)
        }
    };
    // `inner_classes_jump_to_outer`: the outer index of the first entry whose
    // inner class has `class`'s name; `None` for 0 or no such entry.
    let jump_to_outer = |class: u16| -> Option<u16> {
        let wanted = name(class)?;
        entries
            .iter()
            .find(|e| name(e.inner_class_info_index) == Some(wanted))
            .map(|e| e.outer_class_info_index)
    };
    // `inner_classes_check_loop_through_outer` (Floyd's walk over outers).
    let circular_from = |entry: &InnerClassInfo| -> bool {
        let mut slow = entry.inner_class_info_index;
        let mut fast = entry.outer_class_info_index;
        // Bounded: a chain cannot be longer than the table without a loop,
        // which the walk then reports.
        for _ in 0..=entries.len() * 2 + 2 {
            if fast == 0 {
                return false;
            }
            if slow != 0 && name(slow).is_some() && name(slow) == name(fast) {
                return true;
            }
            let Some(next) = jump_to_outer(fast) else {
                return false;
            };
            fast = next;
            if fast == 0 {
                return false;
            }
            let Some(next) = jump_to_outer(fast) else {
                return false;
            };
            fast = next;
            slow = jump_to_outer(slow).unwrap_or(0);
        }
        false
    };
    for (i, entry) in entries.iter().enumerate() {
        if circular_from(entry) {
            return InnerClassesVerdict::Ignore;
        }
        for later in &entries[i + 1..] {
            if later.inner_class_info_index == entry.inner_class_info_index
                && later.outer_class_info_index == entry.outer_class_info_index
                && later.inner_name_index == entry.inner_name_index
                && later.inner_class_access_flags == entry.inner_class_access_flags
            {
                return InnerClassesVerdict::DuplicateEntry;
            }
            if later.inner_class_info_index == entry.inner_class_info_index {
                return InnerClassesVerdict::Ignore;
            }
        }
    }
    InnerClassesVerdict::Keep
}

#[cfg(test)]
mod inner_classes_verdict_tests;

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

// ---------------------------------------------------------------------------
// Lazy attribute decoding (T11)
// ---------------------------------------------------------------------------
//
// Per the audit, ~70% of attributes on java.base bootstrap are never consumed
// (LocalVariableTable, LocalVariableTypeTable, RuntimeInvisibleAnnotations,
// Module, ...). `LazyAttribute` lets the class reader keep raw bytes around
// for an attribute and only pay the parse cost when a downstream consumer
// actually asks for the structured value.
//
// **Eager-decode candidates considered (final recommendation: all lazy):**
//
// * `Code` — needed by JIT/verifier the *first* time a method is invoked,
//   not at class load. Lazy is a clear win: bootstrap loads thousands of
//   methods that are never called.
// * `ConstantValue` — `<clinit>` synthesis reads it once per static field
//   that has one. The downstream class loader can call `decode()` then.
// * `Exceptions` — only consulted when checking `throws` clauses at link
//   time or during stack unwinding. Lazy.
// * `StackMapTable` — only consumed by the verifier; we already store its
//   contents as raw bytes inside `Attribute::StackMapTable`, so the
//   "decode" cost is essentially a memcpy anyway.
// * `BootstrapMethods` — only needed when an `invokedynamic` is resolved.
//   Lazy.
//
// In every case the consumer (`classloading/src/class.rs` for class-level
// attributes, the method/field record builders for member-level) can call
// `decode()` at the exact point of need. Keeping everything lazy by default
// gives the consumer a uniform API and avoids burning startup time on
// attributes that are statistically rarely read.

/// Lazy attribute container. Initially holds raw bytes and the attribute
/// name; decodes to a typed [`Attribute`] on first access via
/// [`LazyAttribute::decode`].
///
/// This is an *additive* wrapper around [`Attribute`]: the existing
/// [`Attribute`] enum and its variants are unchanged. Producers (the class
/// reader) construct `LazyAttribute::Raw` with the raw attribute body bytes;
/// consumers (the class loader, JIT, verifier) call [`decode`] to obtain a
/// `&Attribute` exactly when they need it. Producers that already have a
/// parsed value (synthetic attributes, test fixtures, AOT caches) can use
/// [`new_decoded`] to bypass parsing entirely.
///
/// The `name` is an `Arc<str>` rather than a `String` because UTF-8 entries
/// in the constant pool are already interned via `cratonvm_types::intern_arc`
/// at parse time (see T10.2); cloning the name is a single refcount bump
/// rather than a new allocation.
///
/// [`decode`]: LazyAttribute::decode
/// [`new_decoded`]: LazyAttribute::new_decoded
#[derive(Debug, Clone)]
pub enum LazyAttribute {
    /// Undecoded: name + a (refcounted) reference into the original class
    /// file buffer plus the byte range that contains this attribute's body.
    ///
    /// The `source` is an `Arc<[u8]>` shared with every other lazy attribute
    /// produced from the same class file: cloning a `LazyAttribute` is one
    /// refcount bump on `name`, one refcount bump on `source`, and a
    /// `Range` copy — no body bytes are duplicated. The first lazy
    /// attribute for a class file pins the entire class buffer alive; when
    /// the last lazy attribute (or its decoded descendant retaining a
    /// reference) drops, the backing buffer is freed.
    ///
    /// The range is `[start, end)` within `source`; `source[range.clone()]`
    /// is exactly the `attribute_length` body bytes, *excluding* the
    /// `attribute_name_index` and `attribute_length` header fields.
    ///
    /// Boxed (zr z11/md, 2026-10-13): the three fields are 56 bytes, and a
    /// `LazyAttribute` slot lives for the class's lifetime in every member's
    /// attribute vector, although every production slot is `Decoded` once the
    /// class is defined. Boxed, a slot is the 32-byte [`Attribute`] (the box
    /// takes one of the attribute's spare tag values); the box is one
    /// short-lived allocation per attribute between parse and decode.
    Raw(Box<RawAttribute>),
    /// Decoded: parsed [`Attribute`] variant.
    Decoded(Attribute),
}

/// An undecoded attribute body, boxed inside [`LazyAttribute::Raw`].
#[derive(Debug, Clone)]
pub struct RawAttribute {
    /// The attribute name, e.g. `"Code"`, `"SourceFile"`. Shared
    /// `Arc<str>` from the constant pool's string pool.
    pub name: Arc<str>,
    /// The shared backing buffer (typically the whole class file).
    pub source: SharedBytes,
    /// Byte range within `source` containing the attribute body.
    pub range: Range<usize>,
    /// An attribute HotSpot's `ClassFileParser` skips in this table
    /// (`class_reader::hotspot_reads_attribute`; interpreter round i5 wave
    /// 111, lane L5): a body that does not decode becomes an opaque
    /// [`Attribute::Unknown`] instead of an error.
    pub lenient: bool,
}

impl LazyAttribute {
    /// Construct a `Raw` variant from a shared source buffer + byte range.
    ///
    /// This is the zero-copy constructor: callers that already hold the
    /// class file as an `Arc<[u8]>` clone the Arc (refcount bump, no
    /// memcpy) and hand it in alongside the body's `start..end` range.
    pub fn new_raw_in(name: Arc<str>, source: impl Into<SharedBytes>, range: Range<usize>) -> Self {
        let source = source.into();
        debug_assert!(
            range.end <= source.len(),
            "LazyAttribute::new_raw_in: range {:?} exceeds source len {}",
            range,
            source.len()
        );
        debug_assert!(
            range.start <= range.end,
            "LazyAttribute::new_raw_in: empty/inverted range {:?}",
            range
        );
        LazyAttribute::Raw(Box::new(RawAttribute {
            name,
            source,
            range,
            lenient: false,
        }))
    }

    /// [`new_raw_in`](LazyAttribute::new_raw_in) for an attribute HotSpot
    /// skips in its table ([`RawAttribute::lenient`]).
    pub fn new_raw_lenient_in(
        name: Arc<str>,
        source: impl Into<SharedBytes>,
        range: Range<usize>,
    ) -> Self {
        let mut attr = Self::new_raw_in(name, source, range);
        if let LazyAttribute::Raw(raw) = &mut attr {
            raw.lenient = true;
        }
        attr
    }

    /// Construct from owned attribute body bytes.
    ///
    /// Wraps the `Vec<u8>` in a fresh `Arc<[u8]>` and points at the whole
    /// thing. Preserved as a convenience for callers (tests, synthetic
    /// attribute construction) that already have owned bytes; the
    /// production class reader path uses [`new_raw_in`] to avoid the
    /// allocation entirely.
    ///
    /// [`new_raw_in`]: LazyAttribute::new_raw_in
    pub fn new_raw(name: Arc<str>, bytes: Vec<u8>) -> Self {
        let source = SharedBytes::from(bytes);
        let len = source.len();
        Self::new_raw_in(name, source, 0..len)
    }

    /// Construct from an already-decoded [`Attribute`].
    ///
    /// Useful for synthetic attributes (e.g. attributes added by the class
    /// loader itself) or test fixtures, where there is never a raw byte
    /// form.
    pub fn new_decoded(attr: Attribute) -> Self {
        LazyAttribute::Decoded(attr)
    }

    /// The attribute's name (cheap, no decode).
    ///
    /// In `Raw` form returns the stored name directly. In `Decoded` form
    /// derives the canonical name from the [`Attribute`] variant.
    pub fn name(&self) -> &str {
        match self {
            LazyAttribute::Raw(raw) => &raw.name,
            LazyAttribute::Decoded(attr) => attribute_canonical_name(attr),
        }
    }

    /// Force decode if not already decoded, then return the decoded
    /// attribute. Transitions `Raw` → `Decoded` in place; subsequent calls
    /// are no-ops and return the cached value.
    ///
    /// The constant pool is required because most attribute bodies contain
    /// indices into the constant pool that the decoder resolves to string
    /// values during parsing (e.g. `SourceFile`, `Signature`).
    pub fn decode(&mut self, constant_pool: &ConstantPool) -> Result<&Attribute, ClassReaderError> {
        // Two-phase to satisfy the borrow checker: read the name + source +
        // range first (immutable borrow of `self`), drop that borrow, then
        // assign the Decoded variant back into `self`. We hold cloned
        // `Arc<str>` + `Arc<[u8]>` (refcount bumps, no allocation) and a
        // snapshot of the range; all remain valid for the duration of
        // `decode_attribute_with_source`.
        if let LazyAttribute::Raw(raw) = self {
            let name = raw.name.clone();
            let source = raw.source.clone();
            let range = raw.range.clone();
            let lenient = raw.lenient;
            // Round 7 audit fix (MED #7): route through the `Arc<str>`
            // entrypoint so the body dispatch can use `Arc::ptr_eq`
            // against canonical interned names — skips one intern
            // lookup per lazy decode (the hot path on bootstrap).
            let decoded =
                match decode_attribute_with_source_arc(&name, &source, range.clone(), constant_pool) {
                    Ok(decoded) => decoded,
                    Err(e) if !lenient || nesting_depth_refusal(&e) => return Err(e),
                    Err(_) => Attribute::Unknown {
                        name,
                        data: Box::new(ByteView::try_new(source, range)?),
                    },
                };
            *self = LazyAttribute::Decoded(decoded);
        }
        match self {
            LazyAttribute::Decoded(attr) => Ok(attr),
            LazyAttribute::Raw { .. } => unreachable!("decoded above"),
        }
    }

    /// Get the decoded attribute if already decoded; `None` otherwise.
    ///
    /// This never performs decoding. Use [`decode`] if you want to force
    /// decoding.
    ///
    /// [`decode`]: LazyAttribute::decode
    pub fn as_decoded(&self) -> Option<&Attribute> {
        match self {
            LazyAttribute::Decoded(attr) => Some(attr),
            LazyAttribute::Raw { .. } => None,
        }
    }

    /// Return the decoded attribute, decoding a `Raw` body on the fly (without
    /// mutating `self`) when it has not been eagerly decoded yet.
    ///
    /// Unlike [`as_decoded`] (which silently yields `None` for a `Raw`
    /// attribute) and [`decode`] (which needs `&mut self` to cache the result),
    /// this works behind a shared `&self` borrow — e.g. a reader-locked class
    /// store — and never depends on a prior `force_decode_all` having run. The
    /// already-decoded case borrows; the on-the-fly case returns an owned value.
    /// Returns `None` only if the raw body fails to decode.
    ///
    /// [`as_decoded`]: LazyAttribute::as_decoded
    /// [`decode`]: LazyAttribute::decode
    pub fn decoded_or_decode<'a>(
        &'a self,
        constant_pool: &ConstantPool,
    ) -> Option<std::borrow::Cow<'a, Attribute>> {
        match self {
            LazyAttribute::Decoded(attr) => Some(std::borrow::Cow::Borrowed(attr)),
            LazyAttribute::Raw(raw) => decode_attribute_with_source_arc(
                &raw.name,
                &raw.source,
                raw.range.clone(),
                constant_pool,
            )
            .ok()
            .map(std::borrow::Cow::Owned),
        }
    }

    /// Is this attribute already decoded?
    pub fn is_decoded(&self) -> bool {
        matches!(self, LazyAttribute::Decoded(_))
    }
}

/// SBR wave 5 (reflmeta): the BODY bytes (JVMS 4.7.16-4.7.22, no name index
/// or length) of a decoded annotation attribute --
/// `RuntimeVisible`/`InvisibleAnnotations`, `...ParameterAnnotations`,
/// `...TypeAnnotations`, `AnnotationDefault` -- with every constant pool index
/// written as read, so the bytes index the class's own pool. What HotSpot
/// hands reflection (`Class.getRawTypeAnnotations`, a member's `annotations`
/// field) and the real `AnnotationParser` reads. `None` for any other
/// attribute.
pub fn annotation_attribute_body(attr: &Attribute) -> Option<Vec<u8>> {
    fn u2(out: &mut Vec<u8>, v: usize) -> Option<()> {
        out.extend_from_slice(&u16::try_from(v).ok()?.to_be_bytes());
        Some(())
    }
    fn ann(out: &mut Vec<u8>, a: &Annotation) -> Option<()> {
        out.extend_from_slice(&a.type_index.to_be_bytes());
        u2(out, a.element_value_pairs.len())?;
        for p in &a.element_value_pairs {
            out.extend_from_slice(&p.element_name_index.to_be_bytes());
            ev(out, &p.value)?;
        }
        Some(())
    }
    fn ev(out: &mut Vec<u8>, v: &ElementValue) -> Option<()> {
        match v {
            ElementValue::Const {
                tag,
                const_value_index,
            } => {
                out.push(*tag);
                out.extend_from_slice(&const_value_index.to_be_bytes());
            }
            ElementValue::Enum {
                type_name_index,
                const_name_index,
            } => {
                out.push(b'e');
                out.extend_from_slice(&type_name_index.to_be_bytes());
                out.extend_from_slice(&const_name_index.to_be_bytes());
            }
            ElementValue::Class { class_info_index } => {
                out.push(b'c');
                out.extend_from_slice(&class_info_index.to_be_bytes());
            }
            ElementValue::AnnotationValue(a) => {
                out.push(b'@');
                ann(out, a)?;
            }
            ElementValue::Array(vs) => {
                out.push(b'[');
                u2(out, vs.len())?;
                for v in vs {
                    ev(out, v)?;
                }
            }
        }
        Some(())
    }
    let mut out = Vec::new();
    match attr {
        Attribute::RuntimeVisibleAnnotations(anns)
        | Attribute::RuntimeInvisibleAnnotations(anns) => {
            u2(&mut out, anns.len())?;
            for a in anns {
                ann(&mut out, a)?;
            }
        }
        Attribute::RuntimeVisibleParameterAnnotations(params)
        | Attribute::RuntimeInvisibleParameterAnnotations(params) => {
            out.push(u8::try_from(params.len()).ok()?);
            for anns in params {
                u2(&mut out, anns.len())?;
                for a in anns {
                    ann(&mut out, a)?;
                }
            }
        }
        Attribute::RuntimeVisibleTypeAnnotations(tas)
        | Attribute::RuntimeInvisibleTypeAnnotations(tas) => {
            u2(&mut out, tas.len())?;
            for ta in tas {
                out.push(ta.target_type);
                out.extend_from_slice(&ta.target_info);
                out.push(u8::try_from(ta.type_path.len()).ok()?);
                for step in &ta.type_path {
                    out.push(step.type_path_kind);
                    out.push(step.type_argument_index);
                }
                ann(&mut out, &ta.annotation)?;
            }
        }
        Attribute::AnnotationDefault(v) => ev(&mut out, v)?,
        _ => return None,
    }
    Some(out)
}

/// Force-decode every [`LazyAttribute`] in the slice that is still in the
/// `Raw` state. Already-decoded entries are skipped.
///
/// Intended for callers that want eager decoding semantics (cached/serialised
/// class data, AOT pipelines, debug tooling), or for code paths that prefer
/// to fail fast on malformed attributes at class-load time rather than at
/// first-use time.
pub fn force_decode_all(
    attrs: &mut [LazyAttribute],
    cp: &ConstantPool,
) -> Result<(), ClassReaderError> {
    for attr in attrs.iter_mut() {
        attr.decode(cp)?;
    }
    Ok(())
}

/// The refusal of a `Code` attribute whose `code_length` is outside
/// `1..=65535`: HotSpot `ClassFileParser::parse_method` words it `Invalid
/// method Code length N in class file C` (interpreter round i5 wave 109, lane
/// L5); the reader's words stay the `Display`.
fn code_length_refusal(code_length: usize) -> ClassReaderError {
    const MAX_CODE_LENGTH: usize = crate::limits::MAX_CODE_LENGTH;
    ClassReaderError::class_file_parser(
        format!("Invalid method Code length {code_length}"),
        ClassReaderError::InvalidClassData {
            message: format!(
                "Code attribute code_length {code_length} outside valid range 1..={MAX_CODE_LENGTH}"
            ),
        },
    )
}

/// HotSpot `parse_method`'s `Code segment has wrong length in class file C`:
/// the `Code` attribute's structured contents do not end where its
/// `attribute_length` says (interpreter round i5 wave 109, lane L5).
fn code_segment_wrong_length(reader: ClassReaderError) -> ClassReaderError {
    ClassReaderError::class_file_parser("Code segment has wrong length", reader)
}

/// Eagerly validate the *shape* of a small, fixed set of attribute kinds
/// whose laziness would otherwise hide structural errors until first
/// downstream access. See the module-level "Validation policy" docs for
/// the rationale and the exact kinds covered.
///
/// `name` is the canonical attribute name resolved from the constant pool;
/// `body` is the raw attribute payload (exactly `attribute_length` bytes,
/// *excluding* the `attribute_name_index` / `attribute_length` header).
///
/// Returns `Ok(())` for all attribute kinds not in the eager-validate set
/// (their structural errors remain deferred to lazy decode). Returns
/// `Err(ClassReaderError::InvalidClassData)` for shape violations in the
/// covered kinds.
///
/// This intentionally does *not* materialise the parsed attribute — the
/// goal is to surface obvious malformed-input errors at `read_class` time
/// while leaving the lazy decode in place for the cases that actually
/// benefit from it. For `Code` the walk recurses into nested attributes
/// (since their shape is part of the `Code` body's well-formedness) but
/// still only performs the same shallow per-kind checks.
/// Validate one `exception_table` entry's program counters against JVMS
/// §4.7.3.
///
/// The spec constrains the three PCs in an `exception_table` entry:
///
/// * "The value of the `start_pc` item must be a valid index into the
///   `code` array of the opcode of an instruction."  → `start_pc <
///   code_length`.
/// * "The value of the `end_pc` item either must be a valid index into the
///   `code` array of the opcode of an instruction, or must be equal to
///   `code_length`."  → `end_pc <= code_length`.
/// * "The value of `start_pc` must be less than the value of `end_pc`."
/// * "The value of the `handler_pc` item ... must be a valid index into the
///   `code` array and must be the index of the opcode of an instruction."
///   → `handler_pc < code_length`.
///
/// This is the same set HotSpot's `ClassFileParser::parse_exception_table`
/// enforces, and it is enforced there for the same reason: `handler_pc` is
/// a *jump target*. The JIT reads it straight out of this table (see
/// `jit/src/lib.rs`, `local_handler_reads_unsafe_local` — `let handler_pc =
/// entry.handler_pc as usize;` fed into a code walk) and the interpreter
/// uses it to reposition `pc` when an exception unwinds. An unchecked
/// `handler_pc` of `0xFFFF` in a 4-byte method is therefore an
/// attacker-chosen out-of-range bytecode index reaching code that has every
/// right to assume the reader already rejected it.
///
/// # What this deliberately does NOT check
///
/// The spec's stronger requirement — that each PC be the index of an
/// **opcode**, not the middle of a multi-byte instruction — needs the
/// bytecode to be decoded first. The reader stores `code` as an
/// undecoded [`ByteView`]; instruction boundaries are established later by
/// `crate::quickened` (which builds the pc→index table) and checked by the
/// bytecode verifier. Enforcing boundary alignment here would mean
/// decoding every method body at parse time, which is precisely the cost
/// the lazy/quickened split exists to avoid. The range checks below are
/// the part that can be done for free, and they are what turn an
/// out-of-bounds index into a parse error instead of a downstream
/// assumption violation.
fn validate_exception_range(
    entry_index: usize,
    start_pc: u16,
    end_pc: u16,
    handler_pc: u16,
    code_length: usize,
) -> Result<(), ClassReaderError> {
    // HotSpot `ClassFileParser::parse_exception_table` (JDK 25) words the
    // three range refusals as one, `Illegal exception table range in class
    // file C` (`start_pc < end_pc && end_pc <= code_length`; a `start_pc` past
    // the code fails one of the two), and the handler's as `Illegal exception
    // table handler in class file C`; the reader's words stay the `Display`
    // (interpreter round i5 wave 109, lane L5).
    const RANGE: &str = "Illegal exception table range";
    let range = |message: String| {
        ClassReaderError::class_file_parser(RANGE, ClassReaderError::InvalidClassData { message })
    };
    let (start, end, handler) = (start_pc as usize, end_pc as usize, handler_pc as usize);
    if start >= code_length {
        return Err(range(format!(
            "Code exception_table[{entry_index}]: start_pc {start} is not a valid index into a code array of length {code_length} (JVMS §4.7.3)"
        )));
    }
    if end > code_length {
        return Err(range(format!(
            "Code exception_table[{entry_index}]: end_pc {end} exceeds code_length {code_length} (JVMS §4.7.3)"
        )));
    }
    if start >= end {
        return Err(range(format!(
            "Code exception_table[{entry_index}]: start_pc {start} must be less than end_pc {end} (JVMS §4.7.3)"
        )));
    }
    if handler >= code_length {
        return Err(ClassReaderError::class_file_parser(
            "Illegal exception table handler",
            ClassReaderError::InvalidClassData {
                message: format!(
                    "Code exception_table[{entry_index}]: handler_pc {handler} is not a valid index into a code array of length {code_length} (JVMS §4.7.3)"
                ),
            },
        ));
    }
    Ok(())
}

/// Validate an `exception_table` entry's `catch_type` against JVMS §4.7.3.
///
/// "If the value of the `catch_type` item is nonzero, it must be a valid
/// index into the `constant_pool` table. The `constant_pool` entry at that
/// index must be a `CONSTANT_Class_info` structure representing a class of
/// exceptions." A zero `catch_type` is the `finally` / `any` handler and is
/// legal.
///
/// Checking this at parse time also closes the two constant-pool index
/// hazards for this field at once: index `0` is the reserved sentinel (a
/// [`ConstantPoolEntry::Tombstone`]) and so is the *second slot* of a
/// `CONSTANT_Long` / `CONSTANT_Double`. Both are stored as `Tombstone`, so
/// the "must be a `ClassReference`" test rejects them without needing a
/// separate case.
///
/// [`ConstantPoolEntry::Tombstone`]: crate::constant_pool::ConstantPoolEntry::Tombstone
fn validate_catch_type(
    entry_index: usize,
    catch_type: u16,
    cp: &ConstantPool,
) -> Result<(), ClassReaderError> {
    if catch_type == 0 {
        // The `finally` / catch-any handler.
        return Ok(());
    }
    match cp.get(catch_type) {
        Some(crate::constant_pool::ConstantPoolEntry::ClassReference { .. }) => Ok(()),
        // HotSpot `parse_exception_table`'s `valid_klass_reference_at`
        // refusal (interpreter round i5 wave 109, lane L5).
        _ => Err(ClassReaderError::class_file_parser(
            "Catch type in exception table has bad constant type",
            ClassReaderError::InvalidConstantPool {
                index: catch_type,
                message: format!(
                    "Code exception_table[{entry_index}]: catch_type must be zero or reference a CONSTANT_Class entry (JVMS §4.7.3)"
                ),
            },
        )),
    }
}

pub fn validate_attribute_shape(name: &str, body: &[u8]) -> Result<(), ClassReaderError> {
    match name {
        // Fixed-size 2-byte cp-index attributes (JVMS §4.7.2 / §4.7.27 /
        // §4.7.28). HotSpot rejects mismatched attribute_length here with
        // java.lang.ClassFormatError — match that behaviour.
        // HotSpot's texts (`parse_field_attributes`,
        // `parse_classfile_attributes`; interpreter round i5 wave 109, lane
        // L5); `ModuleMainClass` is not a `ClassFileParser` attribute.
        "ConstantValue" | "NestHost" | "ModuleMainClass" => {
            if body.len() != 2 {
                let reader = ClassReaderError::InvalidClassData {
                    message: format!(
                        "{name} attribute must have attribute_length=2, got {}",
                        body.len()
                    ),
                };
                return Err(match name {
                    "ConstantValue" => ClassReaderError::class_file_parser(
                        format!("Invalid ConstantValue field attribute length {}", body.len()),
                        reader,
                    ),
                    "NestHost" => {
                        ClassReaderError::class_file_parser("Wrong NestHost attribute length", reader)
                    }
                    _ => reader,
                });
            }
        }
        // `u2 count; u2 classes[count]`: HotSpot's `Wrong NestMembers
        // attribute length` / `Wrong PermittedSubclasses attribute length`
        // (`parse_classfile_attributes`; interpreter round i5 wave 109, lane
        // L5). The lazy decoder refused a mismatched body too, later and in
        // its own words.
        "NestMembers" | "PermittedSubclasses" => {
            let declared = body
                .get(..2)
                .map(|c| 2 + 2 * usize::from(u16::from_be_bytes([c[0], c[1]])));
            if declared != Some(body.len()) {
                return Err(ClassReaderError::class_file_parser(
                    format!("Wrong {name} attribute length"),
                    ClassReaderError::InvalidClassData {
                        message: format!(
                            "{name} attribute_length {} does not match its class count",
                            body.len()
                        ),
                    },
                ));
            }
        }
        // HotSpot's `Wrong SourceFile attribute length` and `Exceptions
        // attribute has wrong length` (`parse_classfile_attributes`,
        // `parse_method`; interpreter round i5 wave 110, lane L5). The lazy
        // decoder refused both bodies too, later and in its own words.
        "SourceFile" => {
            if body.len() != 2 {
                return Err(ClassReaderError::class_file_parser(
                    "Wrong SourceFile attribute length",
                    ClassReaderError::InvalidClassData {
                        message: format!(
                            "SourceFile attribute must have attribute_length=2, got {}",
                            body.len()
                        ),
                    },
                ));
            }
        }
        "Exceptions" => {
            let declared = body
                .get(..2)
                .map(|c| 2 + 2 * usize::from(u16::from_be_bytes([c[0], c[1]])));
            if declared != Some(body.len()) {
                return Err(ClassReaderError::class_file_parser(
                    "Exceptions attribute has wrong length",
                    ClassReaderError::InvalidClassData {
                        message: format!(
                            "Exceptions attribute_length {} does not match its count",
                            body.len()
                        ),
                    },
                ));
            }
        }
        // HotSpot `parse_classfile_bootstrap_methods_attribute`'s length
        // refusals (interpreter round i5 wave 111, lane L5): fewer than 2
        // bytes, an entry or argument list running past the body, and bytes
        // left after the last entry. The lazy decoder refused all three in
        // its own words.
        "BootstrapMethods" => {
            if body.len() < 2 {
                return Err(ClassReaderError::class_file_parser(
                    format!("Invalid BootstrapMethods attribute length {}", body.len()),
                    ClassReaderError::InvalidClassData {
                        message: "BootstrapMethods attribute has no count".to_string(),
                    },
                ));
            }
            let count = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let mut at = 2usize;
            for _ in 0..count {
                let argc = body
                    .get(at + 2..at + 4)
                    .map(|c| usize::from(u16::from_be_bytes([c[0], c[1]])));
                match argc {
                    Some(argc) if at + 4 + 2 * argc <= body.len() => at += 4 + 2 * argc,
                    _ => {
                        return Err(ClassReaderError::class_file_parser(
                            "Invalid BootstrapMethods num_bootstrap_methods or num_bootstrap_arguments value",
                            ClassReaderError::InvalidClassData {
                                message: "BootstrapMethods entry runs past the attribute"
                                    .to_string(),
                            },
                        ));
                    }
                }
            }
            if at != body.len() {
                return Err(ClassReaderError::class_file_parser(
                    "Bad length on BootstrapMethods",
                    ClassReaderError::InvalidClassData {
                        message: format!(
                            "BootstrapMethods attribute has {} trailing bytes",
                            body.len() - at
                        ),
                    },
                ));
            }
        }
        // HotSpot `parse_classfile_attributes`' `Record attribute has wrong
        // length` (the components' structure, their attribute headers and
        // bodies, must end exactly where the attribute does; interpreter
        // round i5 wave 112, lane L5). The lazy decoder refused a mismatch
        // too, later and in its own words.
        "Record" => {
            let u2_at = |at: usize| {
                body.get(at..at + 2)
                    .map(|c| usize::from(u16::from_be_bytes([c[0], c[1]])))
            };
            let fits = (|| -> Option<bool> {
                let count = u2_at(0)?;
                let mut at = 2usize;
                for _ in 0..count {
                    let attributes = u2_at(at + 4)?;
                    at += 6;
                    for _ in 0..attributes {
                        let len = body.get(at + 2..at + 6)?;
                        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]);
                        at = at.checked_add(6)?.checked_add(usize::try_from(len).ok()?)?;
                        if at > body.len() {
                            return None;
                        }
                    }
                }
                Some(at == body.len())
            })();
            if fits != Some(true) {
                return Err(ClassReaderError::class_file_parser(
                    "Record attribute has wrong length",
                    ClassReaderError::InvalidClassData {
                        message: format!(
                            "Record attribute_length {} does not match its components",
                            body.len()
                        ),
                    },
                ));
            }
        }
        // Fixed-size 4-byte attribute (JVMS §4.7.7).
        "EnclosingMethod" => {
            if body.len() != 4 {
                return Err(ClassReaderError::class_file_parser(
                    format!("Wrong EnclosingMethod attribute length {}", body.len()),
                    ClassReaderError::InvalidClassData {
                        message: format!(
                            "EnclosingMethod attribute must have attribute_length=4, got {}",
                            body.len()
                        ),
                    },
                ));
            }
        }
        // JVMS §4.7.3: `Code_attribute { u2 max_stack; u2 max_locals; u4
        // code_length; u1 code[code_length]; u2 exception_table_length;
        // {...} exception_table[exception_table_length]; u2
        // attributes_count; attribute_info attributes[attributes_count]; }`.
        // We validate `code_length ∈ 1..=65535`, that the body has enough
        // bytes for code + exception table + nested attribute headers, and
        // that nested attributes themselves are well-shaped (recursively
        // shape-checked through the same predicate).
        // A read past the end of the `Code` body is HotSpot's `Code segment
        // has wrong length` (it reads on past the attribute, then compares the
        // lengths; interpreter round i5 wave 109, lane L5).
        "Code" => validate_code_shape(body).map_err(|e| match e {
            e @ ClassReaderError::UnexpectedEndOfData { .. } => code_segment_wrong_length(e),
            other => other,
        })?,
        _ => {
            // Every other attribute kind keeps its full validation
            // deferred to lazy decode (see the policy comment at the
            // top of this file).
        }
    }
    Ok(())
}

/// [`validate_attribute_shape`]'s `Code` arm.
fn validate_code_shape(body: &[u8]) -> Result<(), ClassReaderError> {
    let mut buf = ClassFileBuffer::new(body);
    // max_stack + max_locals + code_length headers.
    let _max_stack = buf.read_u16()?;
    let _max_locals = buf.read_u16()?;
    let code_length = wire_len_to_usize("Code code_length", buf.read_u32()?)?;
    const MAX_CODE_LENGTH: usize = crate::limits::MAX_CODE_LENGTH;
    if code_length == 0 || code_length > MAX_CODE_LENGTH {
        return Err(code_length_refusal(code_length));
    }
    // `read_bytes` already rejects when the requested span exceeds
    // the remaining buffer, so this catches "code_length larger
    // than the actual body".
    let _ = buf.read_bytes(code_length)?;
    let exception_table_length = buf.read_u16()? as usize;
    const ET_ENTRY_SIZE: usize = EXCEPTION_TABLE_ENTRY_SIZE;
    let et_span = checked_span(
        "Code exception_table",
        exception_table_length,
        ET_ENTRY_SIZE,
    )?;
    let et_bytes = buf.read_bytes(et_span)?;
    // JVMS §4.7.3 program-counter ranges. This walk has no constant
    // pool, so `catch_type` is checked later in `decode_code_body`;
    // the three PCs are checkable here and are the ones that become
    // jump targets downstream. Doing it in the eager shape walk is
    // what makes a hostile `handler_pc` fail at `read_class` time
    // rather than at whatever downstream call site first decodes the
    // method. See `validate_exception_range`.
    for (entry_index, chunk) in et_bytes.chunks_exact(ET_ENTRY_SIZE).enumerate() {
        validate_exception_range(
            entry_index,
            u16::from_be_bytes([chunk[0], chunk[1]]),
            u16::from_be_bytes([chunk[2], chunk[3]]),
            u16::from_be_bytes([chunk[4], chunk[5]]),
            code_length,
        )?;
    }
    // Nested attributes — walk the headers and recurse for shape.
    let attributes_count = buf.read_u16()? as usize;
    for _ in 0..attributes_count {
        // u2 name_index; u4 attribute_length; bytes body.
        // The constant-pool lookup that resolves `name_index` to a
        // Utf8 string isn't available here (we don't take a CP),
        // so we can only check that the header bytes are present
        // and that `attribute_length` does not run past the
        // declared `Code` body. The lazy decoder still performs
        // the full constant-pool-keyed dispatch at first access.
        let _name_index = buf.read_u16()?;
        let nested_len = wire_len_to_usize("nested attribute_length", buf.read_u32()?)?;
        if nested_len > buf.remaining() {
            return Err(code_segment_wrong_length(ClassReaderError::InvalidClassData {
                message: format!(
                    "nested attribute inside Code body declares length {nested_len} but only {} bytes remain",
                    buf.remaining()
                ),
            }));
        }
        // Skip the nested body — we deliberately do *not* recurse
        // into per-kind shape checks here without the constant
        // pool. Header-shape validity is what blocks the
        // misalignment attack; per-kind body shape is re-checked
        // by the lazy decoder when the nested attribute is
        // actually consumed.
        let _ = buf.read_bytes(nested_len)?;
    }
    // Code body must be exactly consumed — trailing junk is a
    // malformed-input signal that the lazy decoder also rejects.
    if buf.remaining() != 0 {
        return Err(code_segment_wrong_length(ClassReaderError::InvalidClassData {
            message: format!(
                "Code attribute body has {} trailing bytes after structured fields",
                buf.remaining()
            ),
        }));
    }
    Ok(())
}

/// Decode a single attribute body. Public so other parts of the class
/// reader (and downstream crates) can opt into structured parsing without
/// going through [`LazyAttribute`].
///
/// `name` is the attribute name (already resolved from the constant pool).
/// `bytes` is *exactly* the attribute body — no `attribute_name_index` or
/// `attribute_length` header. The decoder consumes the bytes via a private
/// [`ClassFileBuffer`] and verifies it consumed all of them; trailing junk
/// or premature EOF is reported as [`ClassReaderError::InvalidClassData`].
///
/// Unknown attribute names produce [`Attribute::Unknown`] with the raw
/// bytes preserved, matching the behaviour of the eager reader.
///
/// **Note:** This entrypoint copies any raw-byte sub-payloads
/// (`Code.code`, `StackMapTable.entries`, `Unknown.data`) into freshly
/// allocated `Arc<[u8]>`s because the caller has no shared source buffer
/// to slice from. The class-reader hot path uses the zero-copy
/// [`decode_attribute_with_source`] variant instead, which slices the
/// shared `Arc<[u8]>` and refcount-bumps it.
pub fn decode_attribute(
    name: &str,
    bytes: &[u8],
    cp: &ConstantPool,
) -> Result<Attribute, ClassReaderError> {
    // Wrap the owned bytes once in a fresh `Arc<[u8]>` so the body
    // decoders that need slices for `Arc<[u8]>` payloads can share the
    // allocation rather than each performing their own `.to_vec()`.
    let source = SharedBytes::from(bytes);
    // Round 7 audit fix (MED #7): the body decoder dispatches on the
    // attribute name via `Arc::ptr_eq` against canonical interned
    // forms. The eager API takes `&str` for back-compat, so intern it
    // here once. The intern table is a global `Mutex<HashMap>` lookup;
    // amortised over the per-attribute parse work it's negligible.
    let name_arc = cratonvm_types::intern_arc(name);
    decode_attribute_with_source_arc(&name_arc, &source, 0..source.len(), cp)
}

/// Zero-copy attribute decoder used by the class-reader hot path.
///
/// `source` is the shared class file buffer; `range` is the inclusive-
/// exclusive byte range of *this attribute's body* inside `source`. Any
/// raw-byte payloads (`Code.code`, `StackMapTable.entries`,
/// `Unknown.data`) are produced as
/// `ByteView::new(Arc::clone(source), sub_range)` — a single atomic
/// refcount bump and two `usize` copies per payload, with no memcpy.
/// (Round-4 wave-2 used `Arc::from(&source[range])` here, which
/// silently allocated a fresh `ArcInner<[u8]>` and memcpy'd the slice;
/// see `round5-reader.md` CRIT-1.)
pub fn decode_attribute_with_source(
    name: &str,
    source: &SharedBytes,
    range: Range<usize>,
    cp: &ConstantPool,
) -> Result<Attribute, ClassReaderError> {
    // Round 7 audit fix (MED #7): forward to the `Arc<str>` variant
    // so the body dispatch can use `Arc::ptr_eq` against canonical
    // names. Eager `&str` callers pay one intern lookup; the lazy
    // hot-path caller already has an interned `Arc<str>` and uses
    // [`decode_attribute_with_source_arc`] directly.
    let name_arc = cratonvm_types::intern_arc(name);
    decode_attribute_with_source_arc(&name_arc, source, range, cp)
}

/// `Arc<str>`-name variant of [`decode_attribute_with_source`] used by
/// the lazy decode hot path. Avoids re-interning the attribute name
/// because the caller already holds the constant-pool-interned arc.
pub fn decode_attribute_with_source_arc(
    name: &Arc<str>,
    source: &SharedBytes,
    range: Range<usize>,
    cp: &ConstantPool,
) -> Result<Attribute, ClassReaderError> {
    debug_assert!(range.end <= source.len());
    debug_assert!(range.start <= range.end);
    let body_offset = range.start;
    let bytes = &source[range.clone()];
    let mut buf = ClassFileBuffer::new(bytes);
    // Top-level (per field/method/class) attribute decoding starts at depth 0;
    // `decode_attribute_body` increments as it descends nested attribute tables
    // and rejects past `MAX_ATTRIBUTE_DEPTH` (audit fix MED: nested-attribute DoS).
    let attr = decode_attribute_body(name, bytes.len(), &mut buf, cp, source, body_offset, 0)?;

    // Enforce that the body parser consumed exactly `bytes.len()` bytes.
    // Anything else indicates a malformed attribute (over-read would have
    // already errored out of the buffer; this catches under-reads / trailing
    // junk, which on the eager path would have mis-aligned the next
    // attribute).
    let consumed = buf.position();
    if consumed != bytes.len() {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "attribute '{name}' body length {len} but parser consumed {consumed} bytes",
                len = bytes.len()
            ),
        });
    }
    Ok(attr)
}

/// Maximum nesting depth for the `decode_attribute_body` ⇄
/// `decode_attributes_vec` (and `decode_code_body`) recursion. A crafted
/// `.class` can nest a `Code` attribute inside a `Code` body, or a
/// `Record` inside a `Record` component's attribute table, arbitrarily
/// deep; without a cap that recursion overflows the native stack and
/// aborts the process (a DoS on untrusted input). 16 far exceeds any
/// attribute nesting a real compiler emits (a `Code` body holds leaf
/// attributes like `LineNumberTable`/`StackMapTable`; legitimate
/// `Record`/`Code` nesting is never more than one or two levels). Mirrors
/// [`MAX_ANNOTATION_DEPTH`] used for the annotation/element-value recursion
/// below. Canonical value lives in [`crate::limits::MAX_ATTRIBUTE_DEPTH`].
const MAX_ATTRIBUTE_DEPTH: usize = crate::limits::MAX_ATTRIBUTE_DEPTH;

/// Decode dispatch — switches on attribute name. Kept separate from
/// [`decode_attribute_with_source`] so the post-parse length check lives
/// in exactly one place. `length` is the total body length, needed by
/// attributes that store raw bytes (`StackMapTable`, `Unknown`).
///
/// `depth` tracks how many nested attribute tables (`Code` → inner table,
/// `Record` → component tables) we've descended through so far. It is
/// checked against [`MAX_ATTRIBUTE_DEPTH`] before recursing to bound the
/// stack on crafted self-nesting input. Top-level (per-field/method/class)
/// attribute decoding starts at depth 0.
///
/// `source` + `body_offset` together pin payload locations inside the
/// shared class file buffer. **Invariant**: `body_offset` is the absolute
/// offset in `source` of *buf-position 0* — that is, the start of the
/// **outermost** attribute body whose bytes the top-level
/// [`decode_attribute_with_source_arc`] sliced into `buf`. Because `buf`
/// is *not* re-sliced when descending into nested attributes (`Code`'s
/// inner table, `Record`'s component tables, …), `buf.position()` is
/// always measured from the outermost body's start, and the absolute
/// source offset of the current buf position is always
/// `body_offset + buf.position()` — at any nesting depth.
///
/// Sub-attributes that store raw bytes use exactly that formula to
/// compute the absolute offset of their payload start inside `source`
/// and wrap it as `ByteView::new(Arc::clone(source), start..end)` — a
/// refcount-only view on the shared buffer instead of a fresh
/// `Vec<u8>` (or, post-round-4-wave-2, a fresh `Arc<[u8]>`) per
/// payload.
///
/// Round-11 fix: prior to this version, the `Code` and `Record` cases
/// each recomputed `body_offset = body_offset + buf.position()` before
/// recursing into [`decode_attributes_vec`], and that function then
/// added `buf.position()` again — double-counting the offset. Any
/// nested `StackMapTable` inside a `Code` body produced a `ByteView`
/// range that ran ~108 bytes past the source length and tripped
/// `ByteView::new`'s bounds assertion at boot.
fn decode_attribute_body(
    name: &Arc<str>,
    length: usize,
    buf: &mut ClassFileBuffer<'_>,
    cp: &ConstantPool,
    source: &SharedBytes,
    body_offset: usize,
    depth: usize,
) -> Result<Attribute, ClassReaderError> {
    // Audit fix (MED): bound nested-attribute recursion. A crafted
    // `.class` can self-nest `Code`-in-`Code` or `Record`-in-`Record`
    // arbitrarily deep (the `Code`/`Record` arms below recurse through
    // `decode_attributes_vec` back into this function). Without a cap that
    // overflows the native stack — a stack-overflow DoS on untrusted
    // input. Reject before recursing, mirroring `MAX_ANNOTATION_DEPTH`.
    if depth >= MAX_ATTRIBUTE_DEPTH {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "attribute '{name}' nesting depth exceeds limit {MAX_ATTRIBUTE_DEPTH}"
            ),
        });
    }
    // Round 7 audit fix (MED #7): Arc-pointer-equality fast path
    // against canonical interned attribute names. Round-3 made the
    // constant pool intern every Utf8 into the global `intern_arc`
    // table, so the `name` arc that lazy-decode hands us is the same
    // refcounted allocation as the canonical names below. A successful
    // `Arc::ptr_eq` is one pointer comparison — substantially cheaper
    // than `str` equality (which loads the length prefix, then either
    // compares 8 bytes at a time or rejects on a length mismatch). The
    // string `match` below remains as a fallback for non-canonical
    // names (vendor / unknown attributes) and for the rare callers
    // that pass an `Arc<str>` that wasn't routed through `intern_arc`.
    //
    // Round 9 audit fix (MED #9): the canonical `LazyLock<Arc<str>>`
    // declarations live at module scope so they're initialized once
    // globally instead of inside every function-scope (still once-only
    // but bloats the function namespace and makes them hard to share).

    // Pick a string discriminant by Arc-ptr-eq first; fall through to
    // the `&**name` deref for the slow path. The selected string is
    // either the canonical static (`Arc::ptr_eq` hit) or the original
    // attribute name (miss). The match below then uses it verbatim.
    let dispatch_name: &str = if Arc::ptr_eq(name, &CANON_CODE) {
        "Code"
    } else if Arc::ptr_eq(name, &CANON_SOURCE_FILE) {
        "SourceFile"
    } else if Arc::ptr_eq(name, &CANON_LINE_NUMBER_TABLE) {
        "LineNumberTable"
    } else if Arc::ptr_eq(name, &CANON_LOCAL_VARIABLE_TABLE) {
        "LocalVariableTable"
    } else if Arc::ptr_eq(name, &CANON_LOCAL_VARIABLE_TYPE_TABLE) {
        "LocalVariableTypeTable"
    } else if Arc::ptr_eq(name, &CANON_STACK_MAP_TABLE) {
        "StackMapTable"
    } else if Arc::ptr_eq(name, &CANON_CONSTANT_VALUE) {
        "ConstantValue"
    } else if Arc::ptr_eq(name, &CANON_EXCEPTIONS) {
        "Exceptions"
    } else if Arc::ptr_eq(name, &CANON_SIGNATURE) {
        "Signature"
    } else if Arc::ptr_eq(name, &CANON_INNER_CLASSES) {
        "InnerClasses"
    } else if Arc::ptr_eq(name, &CANON_BOOTSTRAP_METHODS) {
        "BootstrapMethods"
    } else if Arc::ptr_eq(name, &CANON_DEPRECATED) {
        "Deprecated"
    } else if Arc::ptr_eq(name, &CANON_SYNTHETIC) {
        "Synthetic"
    } else if Arc::ptr_eq(name, &CANON_NEST_HOST) {
        "NestHost"
    } else if Arc::ptr_eq(name, &CANON_NEST_MEMBERS) {
        "NestMembers"
    } else if Arc::ptr_eq(name, &CANON_RUNTIME_VISIBLE_ANNOTATIONS) {
        "RuntimeVisibleAnnotations"
    } else if Arc::ptr_eq(name, &CANON_RUNTIME_INVISIBLE_ANNOTATIONS) {
        "RuntimeInvisibleAnnotations"
    } else if Arc::ptr_eq(name, &CANON_METHOD_PARAMETERS) {
        "MethodParameters"
    } else if Arc::ptr_eq(name, &CANON_ENCLOSING_METHOD) {
        "EnclosingMethod"
    } else if Arc::ptr_eq(name, &CANON_RUNTIME_VISIBLE_PARAMETER_ANNOTATIONS) {
        "RuntimeVisibleParameterAnnotations"
    } else if Arc::ptr_eq(name, &CANON_RUNTIME_INVISIBLE_PARAMETER_ANNOTATIONS) {
        "RuntimeInvisibleParameterAnnotations"
    } else if Arc::ptr_eq(name, &CANON_RUNTIME_VISIBLE_TYPE_ANNOTATIONS) {
        "RuntimeVisibleTypeAnnotations"
    } else if Arc::ptr_eq(name, &CANON_RUNTIME_INVISIBLE_TYPE_ANNOTATIONS) {
        "RuntimeInvisibleTypeAnnotations"
    } else if Arc::ptr_eq(name, &CANON_ANNOTATION_DEFAULT) {
        "AnnotationDefault"
    } else if Arc::ptr_eq(name, &CANON_MODULE) {
        "Module"
    } else if Arc::ptr_eq(name, &CANON_RECORD) {
        "Record"
    } else if Arc::ptr_eq(name, &CANON_PERMITTED_SUBCLASSES) {
        "PermittedSubclasses"
    } else if Arc::ptr_eq(name, &CANON_SOURCE_DEBUG_EXTENSION) {
        "SourceDebugExtension"
    } else if Arc::ptr_eq(name, &CANON_MODULE_PACKAGES) {
        // Round 9 audit fix (HIGH #4): ModulePackages — JPMS module-info.
        "ModulePackages"
    } else if Arc::ptr_eq(name, &CANON_MODULE_MAIN_CLASS) {
        // Round 9 audit fix (HIGH #4): ModuleMainClass — JPMS module-info.
        "ModuleMainClass"
    } else {
        &**name
    };

    let attr = match dispatch_name {
        "Code" => decode_code_body(buf, cp, source, body_offset, depth)?,
        "SourceFile" => {
            let source_file_index = buf.read_u16()?;
            // Fetch the interned `Arc<str>` straight from the constant pool —
            // refcount bump on the shared pool allocation, no fresh String.
            // HotSpot `parse_classfile_sourcefile_attribute`'s text
            // (interpreter round i5 wave 110, lane L5).
            let source_file = cp.get_utf8_arc(source_file_index).ok_or_else(|| {
                ClassReaderError::class_file_parser(
                    format!("Invalid SourceFile attribute at constant pool index {source_file_index}"),
                    ClassReaderError::InvalidConstantPool {
                        index: source_file_index,
                        message: "SourceFile must reference a valid Utf8 entry".to_string(),
                    },
                )
            })?;
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
            // Round 7 audit fix (MED #6 / round-4 #4): bulk slice parse
            // of the u16 cp-index array.
            //
            // C2 remediation: the `count * entry_size` product goes through
            // `checked_span` and the reservation through `bounded_capacity`
            // so neither can be driven past the input length. `read_bytes`
            // is still what proves the bytes are there.
            let num_exceptions = buf.read_u16()? as usize;
            let span = checked_span("Exceptions", num_exceptions, EXCEPTIONS_ENTRY_SIZE)?;
            let capacity = bounded_capacity(num_exceptions, EXCEPTIONS_ENTRY_SIZE, buf.remaining());
            let bytes = buf.read_bytes(span)?;
            let mut exception_indices = Vec::with_capacity(capacity);
            for chunk in bytes.chunks_exact(2) {
                exception_indices.push(u16::from_be_bytes([chunk[0], chunk[1]]));
            }
            Attribute::Exceptions { exception_indices }
        }
        "LineNumberTable" => {
            // Round 7 audit fix (MED #6 / round-4 #4): replace the
            // per-`u16` `read_u16()` loop with a single
            // `read_bytes(4 * count)` slice grab + linear parse from
            // `[u8]`. Each `read_u16()` does an `Option::ok_or(...)?`
            // bounds check + a `try_into().unwrap()` array conversion;
            // for an `N`-entry table that's `2*N` checked reads. The
            // bulk read does ONE bounds check and the inner parse is a
            // tight `u16::from_be_bytes([chunk[0], chunk[1]])`
            // (`unchecked_shl + or`) — 3-4× faster on bootstrap where
            // LineNumberTable is ubiquitous.
            let table_length = buf.read_u16()? as usize;
            const ENTRY_SIZE: usize = LINE_NUMBER_ENTRY_SIZE; // start_pc + line_number
            let span = checked_span("LineNumberTable", table_length, ENTRY_SIZE)?;
            let capacity = bounded_capacity(table_length, ENTRY_SIZE, buf.remaining());
            let bytes = buf.read_bytes(span)?;
            let mut entries = Vec::with_capacity(capacity);
            for chunk in bytes.chunks_exact(ENTRY_SIZE) {
                entries.push(LineNumberEntry {
                    start_pc: u16::from_be_bytes([chunk[0], chunk[1]]),
                    line_number: u16::from_be_bytes([chunk[2], chunk[3]]),
                });
            }
            Attribute::LineNumberTable(entries)
        }
        "InnerClasses" => {
            // Round 7 audit fix (MED #6 / round-4 #4): bulk slice
            // parse — see LineNumberTable comment for rationale.
            let num_classes = buf.read_u16()? as usize;
            const ENTRY_SIZE: usize = INNER_CLASS_ENTRY_SIZE; // four u16 fields
            let span = checked_span("InnerClasses", num_classes, ENTRY_SIZE)?;
            let capacity = bounded_capacity(num_classes, ENTRY_SIZE, buf.remaining());
            let bytes = buf.read_bytes(span)?;
            let mut classes = Vec::with_capacity(capacity);
            for chunk in bytes.chunks_exact(ENTRY_SIZE) {
                classes.push(InnerClassInfo {
                    inner_class_info_index: u16::from_be_bytes([chunk[0], chunk[1]]),
                    outer_class_info_index: u16::from_be_bytes([chunk[2], chunk[3]]),
                    inner_name_index: u16::from_be_bytes([chunk[4], chunk[5]]),
                    inner_class_access_flags: u16::from_be_bytes([chunk[6], chunk[7]]),
                });
            }
            Attribute::InnerClasses(classes)
        }
        "Signature" => {
            let signature_index = buf.read_u16()?;
            // Refcount-bump clone of the pool-interned signature string.
            // HotSpot `parse_generic_signature_attribute`'s text
            // (interpreter round i5 wave 111, lane L5).
            let signature = cp.get_utf8_arc(signature_index).ok_or_else(|| {
                ClassReaderError::class_file_parser(
                    format!("Invalid constant pool index {signature_index} in Signature attribute"),
                    ClassReaderError::InvalidConstantPool {
                        index: signature_index,
                        message: "Signature must reference a valid Utf8 entry".to_string(),
                    },
                )
            })?;
            Attribute::Signature(signature)
        }
        "StackMapTable" => {
            // The StackMapTable body is stored verbatim — verifier-time
            // parsing happens later in the `stack_map` module.
            //
            // True zero-copy: build a `ByteView` over the shared
            // class-file `Arc<[u8]>`. This is a single `Arc::clone`
            // refcount bump plus two `usize` copies — no memcpy. The
            // slice's start is `body_offset + current buffer position`;
            // we still call `read_bytes` to advance the buffer and
            // surface any EOF exactly the way the previous code did.
            //
            // (Round-4 wave-2 used `Arc::from(&source[range])` here,
            // which silently allocated a fresh `ArcInner<[u8]>` and
            // memcpy'd the slice — the parent Arc was *not* shared.
            // See round5-reader.md CRIT-1.)
            let start = checked_end("StackMapTable offset", body_offset, buf.position())?;
            let _ = buf.read_bytes(length)?;
            // Defense-in-depth: even though `validate_attribute_shape`
            // already walks the outer body, derive this slice via
            // `try_new` so an OOB range surfaces as `InvalidClassData`
            // rather than panicking (round-11 off-by-`buf.position()`
            // regression). `checked_end` covers the range arithmetic
            // itself so a wrapped `end` can never present as in-bounds.
            let end = checked_end("StackMapTable range", start, length)?;
            let entries = Box::new(ByteView::try_new(source.clone(), start..end)?);
            Attribute::StackMapTable { entries }
        }
        "BootstrapMethods" => {
            let num_bootstrap_methods = buf.read_u16()?;
            let mut methods =
                Vec::with_capacity((num_bootstrap_methods as usize).min(PREALLOC_CAP));
            for _ in 0..num_bootstrap_methods {
                let bootstrap_method_ref = buf.read_u16()?;
                let num_args = buf.read_u16()?;
                let mut bootstrap_arguments =
                    Vec::with_capacity((num_args as usize).min(PREALLOC_CAP));
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
            let mut components = Vec::with_capacity((num_components as usize).min(PREALLOC_CAP));
            for _ in 0..num_components {
                let comp_name_index = buf.read_u16()?;
                let comp_descriptor_index = buf.read_u16()?;
                // `body_offset` is the absolute offset in `source` of
                // buf-position 0 (the outermost attribute body's start).
                // It stays constant across nesting levels because `buf`
                // is the same buffer; absolute offsets are always
                // `body_offset + buf.position()`. See the comment on
                // [`decode_attribute_body`] for the invariant.
                // Audit fix (MED): `depth + 1` — descending into a Record
                // component's nested attribute table is one more level of
                // nesting; bounds Record-in-Record self-nesting.
                let comp_attributes =
                    decode_attributes_vec(buf, cp, source, body_offset, depth + 1, NestedTable::RecordComponent)?;
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

            Attribute::Module(Box::new(ModuleAttribute {
                name_index,
                flags,
                version_index,
                requires,
                exports,
                opens,
                uses,
                provides,
            }))
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
                annotations.push(decode_annotation(buf)?);
            }
            if dispatch_name == "RuntimeVisibleAnnotations" {
                Attribute::RuntimeVisibleAnnotations(annotations)
            } else {
                Attribute::RuntimeInvisibleAnnotations(annotations)
            }
        }
        "RuntimeVisibleParameterAnnotations" | "RuntimeInvisibleParameterAnnotations" => {
            let num_parameters = buf.read_u8()?;
            let mut parameter_annotations =
                Vec::with_capacity((num_parameters as usize).min(PREALLOC_CAP));
            for _ in 0..num_parameters {
                let num_annotations = buf.read_u16()?;
                let mut annotations =
                    Vec::with_capacity((num_annotations as usize).min(PREALLOC_CAP));
                for _ in 0..num_annotations {
                    annotations.push(decode_annotation(buf)?);
                }
                parameter_annotations.push(annotations);
            }
            if dispatch_name == "RuntimeVisibleParameterAnnotations" {
                Attribute::RuntimeVisibleParameterAnnotations(parameter_annotations)
            } else {
                Attribute::RuntimeInvisibleParameterAnnotations(parameter_annotations)
            }
        }
        "RuntimeVisibleTypeAnnotations" | "RuntimeInvisibleTypeAnnotations" => {
            let num_annotations = buf.read_u16()?;
            let mut annotations = Vec::with_capacity((num_annotations as usize).min(PREALLOC_CAP));
            for _ in 0..num_annotations {
                annotations.push(decode_type_annotation(buf)?);
            }
            if dispatch_name == "RuntimeVisibleTypeAnnotations" {
                Attribute::RuntimeVisibleTypeAnnotations(annotations)
            } else {
                Attribute::RuntimeInvisibleTypeAnnotations(annotations)
            }
        }
        "AnnotationDefault" => {
            let value = decode_element_value(buf)?;
            Attribute::AnnotationDefault(Box::new(value))
        }
        "LocalVariableTable" => {
            // Round 7 audit fix (MED #6 / round-4 #4): bulk slice parse.
            let table_length = buf.read_u16()? as usize;
            const ENTRY_SIZE: usize = LOCAL_VARIABLE_ENTRY_SIZE; // five u16 fields
            let span = checked_span("LocalVariableTable", table_length, ENTRY_SIZE)?;
            let capacity = bounded_capacity(table_length, ENTRY_SIZE, buf.remaining());
            let bytes = buf.read_bytes(span)?;
            let mut entries = Vec::with_capacity(capacity);
            for chunk in bytes.chunks_exact(ENTRY_SIZE) {
                entries.push(LocalVariableEntry {
                    start_pc: u16::from_be_bytes([chunk[0], chunk[1]]),
                    length: u16::from_be_bytes([chunk[2], chunk[3]]),
                    name_index: u16::from_be_bytes([chunk[4], chunk[5]]),
                    descriptor_index: u16::from_be_bytes([chunk[6], chunk[7]]),
                    index: u16::from_be_bytes([chunk[8], chunk[9]]),
                });
            }
            Attribute::LocalVariableTable(entries)
        }
        "LocalVariableTypeTable" => {
            // Round 7 audit fix (MED #6 / round-4 #4): bulk slice parse.
            let table_length = buf.read_u16()? as usize;
            const ENTRY_SIZE: usize = LOCAL_VARIABLE_ENTRY_SIZE; // five u16 fields
            let span = checked_span("LocalVariableTypeTable", table_length, ENTRY_SIZE)?;
            let capacity = bounded_capacity(table_length, ENTRY_SIZE, buf.remaining());
            let bytes = buf.read_bytes(span)?;
            let mut entries = Vec::with_capacity(capacity);
            for chunk in bytes.chunks_exact(ENTRY_SIZE) {
                entries.push(LocalVariableTypeEntry {
                    start_pc: u16::from_be_bytes([chunk[0], chunk[1]]),
                    length: u16::from_be_bytes([chunk[2], chunk[3]]),
                    name_index: u16::from_be_bytes([chunk[4], chunk[5]]),
                    signature_index: u16::from_be_bytes([chunk[6], chunk[7]]),
                    index: u16::from_be_bytes([chunk[8], chunk[9]]),
                });
            }
            Attribute::LocalVariableTypeTable(entries)
        }
        "MethodParameters" => {
            // Round 7 audit fix (MED #6 / round-4 #4): bulk slice
            // parse. `parameters_count` is u8 so the maximum payload
            // is 255*4 = 1020 bytes — a single small alloc.
            let parameters_count = buf.read_u8()? as usize;
            const ENTRY_SIZE: usize = METHOD_PARAMETER_ENTRY_SIZE; // two u16 fields
            let span = checked_span("MethodParameters", parameters_count, ENTRY_SIZE)?;
            let capacity = bounded_capacity(parameters_count, ENTRY_SIZE, buf.remaining());
            let bytes = buf.read_bytes(span)?;
            let mut parameters = Vec::with_capacity(capacity);
            for chunk in bytes.chunks_exact(ENTRY_SIZE) {
                parameters.push(MethodParameter {
                    name_index: u16::from_be_bytes([chunk[0], chunk[1]]),
                    access_flags: u16::from_be_bytes([chunk[2], chunk[3]]),
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
            // Unknown attribute — preserve raw bytes verbatim. Note we use
            // `length` (the total body length) here, not `buf.remaining()`,
            // because the post-parse check in `decode_attribute_with_source`
            // would catch a mismatch anyway.
            //
            // True zero-copy: build a `ByteView` over the shared
            // class-file `Arc<[u8]>` for the data payload, and re-use
            // the pool-interned attribute name (a refcount bump on the
            // same `Arc<str>` the LazyAttribute header carries). On
            // bootstrap, hundreds of distinct vendor / legacy attribute
            // names recur many times across classes — the intern path
            // makes each name a single allocation. (Round-4 wave-2
            // used `Arc::from(&source[range])` here, which allocated a
            // fresh `ArcInner<[u8]>` + memcpy'd the slice; the parent
            // Arc was *not* shared. See round5-reader.md CRIT-1.)
            let start = checked_end("Unknown attribute offset", body_offset, buf.position())?;
            let _ = buf.read_bytes(length)?;
            // Defense-in-depth: see `StackMapTable` arm — runtime-
            // derived offset/length must not panic on OOB.
            let end = checked_end("Unknown attribute range", start, length)?;
            let data = ByteView::try_new(source.clone(), start..end)?;
            // Round 7 audit fix (MED #7): `name` is already an
            // interned `Arc<str>` (the caller passed in the canonical
            // pool-interned arc); just refcount-bump instead of
            // re-interning, saving a global table lookup per Unknown
            // attribute.
            Attribute::Unknown {
                name: Arc::clone(name),
                data: Box::new(data),
            }
        }
    };
    Ok(attr)
}

/// The refusal [`decode_attribute_body`] makes past [`MAX_ATTRIBUTE_DEPTH`]
/// (its message's fixed part, `nesting depth exceeds limit`).
fn nesting_depth_refusal(e: &ClassReaderError) -> bool {
    matches!(e, ClassReaderError::InvalidClassData { message } if message.contains("nesting depth exceeds limit"))
}

/// Decode a nested attributes table (used by `Code` and `Record`).
///
/// Format: `u2 attributes_count` followed by `attributes_count` attribute
/// records. Each record is decoded eagerly here — nested attributes inside
/// a Code body are typically `LineNumberTable`, `LocalVariableTable`,
/// `StackMapTable`, etc. Lazy nesting would be future work.
///
/// `source` + `outer_body_offset` thread the shared class file buffer + the
/// outer attribute body's start through to the per-nested decoder so its
/// raw-byte payloads (`StackMapTable.entries`, `Unknown.data`) can be
/// `ByteView::new(Arc::clone(source), ..)` views instead of fresh
/// `Vec<u8>` (or fresh `Arc<[u8]>`) allocations per payload.
/// Whose nested attribute table [`decode_attributes_vec`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NestedTable {
    /// A `Code` attribute's own table.
    Code,
    /// A `Record` component's table.
    RecordComponent,
}

fn decode_attributes_vec(
    buf: &mut ClassFileBuffer<'_>,
    cp: &ConstantPool,
    source: &SharedBytes,
    outermost_body_offset: usize,
    depth: usize,
    table: NestedTable,
) -> Result<Vec<Attribute>, ClassReaderError> {
    // INVARIANT (round-11 fix): `outermost_body_offset` is the absolute
    // offset in `source` that corresponds to *buf-position 0* — i.e. the
    // start of the **outermost** attribute body that the top-level
    // `decode_attribute_with_source_arc` call sliced the buffer from.
    //
    // The buffer is *not* re-sliced per nesting level, so its position is
    // always measured from the outermost body's start. Therefore the
    // absolute source offset of *any* current buf position is
    // `outermost_body_offset + buf.position()`, regardless of nesting
    // depth. Callers must thread the same outermost offset through unchanged
    // — they must NOT re-add `buf.position()` at each nesting level
    // (doing so double-counts the position and produces a `start..end`
    // range that overshoots the source, panicking
    // `ByteView::new`'s bounds check on attributes like a nested
    // `StackMapTable` inside a `Code` body).
    let count = buf.read_u16()?;
    // An attribute_info costs at least its 6-byte header (u2 name index +
    // u4 length), so the reservation is bounded by the bytes that remain
    // rather than by the declared count.
    const NESTED_ATTRIBUTE_HEADER_BYTES: usize = 6;
    let mut out = Vec::with_capacity(bounded_capacity(
        count as usize,
        NESTED_ATTRIBUTE_HEADER_BYTES,
        buf.remaining(),
    ));
    for _ in 0..count {
        let name_index = buf.read_u16()?;
        // Refcount-bump clone of the pool-interned attribute name — no
        // fresh String per nested attribute.
        let name = cp.get_utf8_arc(name_index).ok_or_else(|| {
            let reader = ClassReaderError::InvalidConstantPool {
                index: name_index,
                message: "nested attribute name must reference a valid Utf8 entry".to_string(),
            };
            // HotSpot `parse_method` / `parse_classfile_record_attribute`
            // texts (interpreter round i5 waves 110-112, lane L5).
            let text = match table {
                NestedTable::Code => format!("Invalid code attribute name index {name_index}"),
                NestedTable::RecordComponent => {
                    format!("Invalid Record attribute name index {name_index}")
                }
            };
            ClassReaderError::class_file_parser(text, reader)
        })?;
        let length = wire_len_to_usize("nested attribute_length", buf.read_u32()?)?;
        if length > buf.remaining() {
            return Err(ClassReaderError::InvalidClassData {
                message: format!(
                    "nested attribute '{name}' length {length} exceeds remaining buffer size {}",
                    buf.remaining()
                ),
            });
        }
        // Snapshot to enforce per-attribute length, mirroring class_reader.rs.
        let start_pos = buf.position();
        // Round 7 audit fix (MED #7): pass the `Arc<str>` directly so
        // the body can dispatch via `Arc::ptr_eq` against canonical
        // names (`LineNumberTable`, `LocalVariableTable`, etc. are the
        // ubiquitous nested attribute inside `Code`).
        //
        // Per the invariant above we pass the *outermost* body offset
        // through unchanged; the nested decoder will compute its own
        // payload's absolute offset as
        // `outermost_body_offset + buf.position()` exactly the same way
        // the top-level decoder does.
        // Audit fix (MED): thread `depth` through unchanged — the caller
        // already incremented it when descending into this nested table, so
        // each nested body is decoded at that level. `decode_attribute_body`
        // rejects once it exceeds `MAX_ATTRIBUTE_DEPTH`.
        // HotSpot's `parse_method` reads only these four in a `Code`
        // attribute's table and skips every other attribute whatever its
        // body; one the reader cannot decode is kept opaque there, not
        // refused (interpreter round i5 wave 111, lane L5; probe
        // `L5W111ClassFileParserLast`, row `ignored-in-code`).
        //
        // Wave 112: the four HotSpot does read go the same way, so a
        // `LineNumberTable` / `LocalVariable(Type)Table` that does not decode
        // is an opaque `Unknown` here, refused with HotSpot's `... has wrong
        // length` by the define path where HotSpot verifies
        // (`class_manager::code_table_refusal`), and a `LocalVariableTypeTable`
        // below version 49 is skipped as HotSpot skips it. A `Record`
        // component's table: HotSpot reads only `Signature` (its length
        // refused in HotSpot's words below) and skips the annotation bodies
        // as bytes.
        if table == NestedTable::RecordComponent && &*name == "Signature" && length != 2 {
            return Err(ClassReaderError::class_file_parser(
                format!("Invalid Signature attribute length {length} in Record component"),
                ClassReaderError::InvalidClassData {
                    message: format!(
                        "Record component Signature attribute must have attribute_length=2, got {length}"
                    ),
                },
            ));
        }
        let lenient = table == NestedTable::Code || &*name != "Signature";
        let attr = if lenient {
            let mut trial = buf.clone();
            match decode_attribute_body(
                &name,
                length,
                &mut trial,
                cp,
                source,
                outermost_body_offset,
                depth,
            ) {
                Ok(decoded) if trial.position() - start_pos == length => {
                    *buf = trial;
                    decoded
                }
                // The nesting cap stays a refusal: it bounds the recursion
                // (a self-nested `Code` chain), not a body HotSpot reads.
                Err(e) if nesting_depth_refusal(&e) => return Err(e),
                _ => {
                    let start = checked_end("nested attribute offset", outermost_body_offset, start_pos)?;
                    buf.skip(length)?;
                    let end = checked_end("nested attribute range", start, length)?;
                    Attribute::Unknown {
                        name: Arc::clone(&name),
                        data: Box::new(ByteView::try_new(source.clone(), start..end)?),
                    }
                }
            }
        } else {
            decode_attribute_body(&name, length, buf, cp, source, outermost_body_offset, depth)?
        };
        let consumed = buf.position() - start_pos;
        if consumed != length {
            return Err(ClassReaderError::InvalidClassData {
                message: format!(
                    "nested attribute '{name}' declared length {length} but sub-parser consumed {consumed} bytes"
                ),
            });
        }
        out.push(attr);
    }
    Ok(out)
}

/// Decode the body of a `Code` attribute (JVM spec 4.7.3). The
/// `attribute_length`/`attribute_name_index` header has already been
/// consumed by the caller.
///
/// `source` + `body_offset` describe the byte range of the Code
/// attribute's body inside the shared class file buffer. The bytecode
/// payload is produced as `ByteView::new(Arc::clone(source),
/// code_start..code_end)` — a true refcount-only view on the shared
/// buffer (single atomic increment, no memcpy) rather than the fresh
/// `Vec<u8>` (pre-round-4) or fresh `Arc<[u8]>` (round-4 wave-2, which
/// turned out to also allocate; see round5-reader.md CRIT-1) that
/// preceded it. On bootstrap (~24 k methods × ~50 B average) this
/// eliminates roughly one allocation + memcpy per method.
fn decode_code_body(
    buf: &mut ClassFileBuffer<'_>,
    cp: &ConstantPool,
    source: &SharedBytes,
    body_offset: usize,
    depth: usize,
) -> Result<Attribute, ClassReaderError> {
    let max_stack = buf.read_u16()?;
    let max_locals = buf.read_u16()?;

    let code_length = wire_len_to_usize("Code code_length", buf.read_u32()?)?;
    // JVM spec 4.7.3: code_length must be > 0 and <= 65535.
    const MAX_CODE_LENGTH: usize = crate::limits::MAX_CODE_LENGTH;
    if code_length == 0 || code_length > MAX_CODE_LENGTH {
        return Err(code_length_refusal(code_length));
    }
    // True zero-copy bytecode: snapshot the absolute offset of the
    // bytecode payload inside `source`, then advance the buffer past
    // it (we still call `read_bytes` for the EOF check). The bytecode
    // [`ByteView`] is a refcount-bumped clone of the shared class-file
    // `Arc<[u8]>` plus a `Range<usize>` — no memcpy. (Round-4 wave-2
    // used `Arc::from(&source[range])` here, which silently allocated
    // a fresh `ArcInner<[u8]>` + memcpy; the parent Arc was *not*
    // shared. See round5-reader.md CRIT-1.)
    let code_start = checked_end("Code offset", body_offset, buf.position())?;
    let _ = buf.read_bytes(code_length)?;
    // Defense-in-depth: the runtime-derived `code_start..code_start +
    // code_length` range goes through `try_new` so a malformed Code
    // body produces `InvalidClassData` instead of aborting the process
    // (the round-11 panic shipped from this very call site). The range
    // arithmetic itself is `checked_end` so it cannot wrap into a range
    // that then passes the bounds test.
    let code_end = checked_end("Code range", code_start, code_length)?;
    let code = ByteView::try_new(source.clone(), code_start..code_end)?;

    // Round 7 audit fix (MED #6 / round-4 #4): bulk slice parse of the
    // ExceptionTable — replaces four per-`u16` `read_u16()` calls per
    // entry with one `read_bytes(8 * N)` slice grab plus a tight inner
    // `u16::from_be_bytes([..])` parse. Each `read_u16` performs a
    // checked-slice bounds check and a `try_into().unwrap()`; the bulk
    // version does one bounds check for the whole table.
    let exception_table_length = buf.read_u16()? as usize;
    const ET_ENTRY_SIZE: usize = EXCEPTION_TABLE_ENTRY_SIZE; // four u16 fields
    let et_span = checked_span(
        "Code exception_table",
        exception_table_length,
        ET_ENTRY_SIZE,
    )?;
    let et_capacity = bounded_capacity(exception_table_length, ET_ENTRY_SIZE, buf.remaining());
    let et_bytes = buf.read_bytes(et_span)?;
    let mut exception_table = Vec::with_capacity(et_capacity);
    for (entry_index, chunk) in et_bytes.chunks_exact(ET_ENTRY_SIZE).enumerate() {
        let entry = ExceptionTableEntry {
            start_pc: u16::from_be_bytes([chunk[0], chunk[1]]),
            end_pc: u16::from_be_bytes([chunk[2], chunk[3]]),
            handler_pc: u16::from_be_bytes([chunk[4], chunk[5]]),
            catch_type: u16::from_be_bytes([chunk[6], chunk[7]]),
        };
        // JVMS §4.7.3. `validate_attribute_shape` already ran the PC range
        // checks on the eager path, but `decode_attribute` is also reachable
        // directly (it is `pub`, and the lazy decoder calls it for bodies
        // that never went through the eager walk — e.g. a `Code` nested
        // inside another attribute), so the authoritative check lives here
        // too. `catch_type` can only be checked here: the eager shape walk
        // has no constant pool.
        validate_exception_range(
            entry_index,
            entry.start_pc,
            entry.end_pc,
            entry.handler_pc,
            code_length,
        )?;
        validate_catch_type(entry_index, entry.catch_type, cp)?;
        exception_table.push(entry);
    }

    // `body_offset` is the absolute source offset of buf-position 0
    // (the outermost attribute body's start). Thread it through unchanged
    // — see the invariant comment on [`decode_attributes_vec`]. The nested
    // decoder will compute absolute offsets as `body_offset + buf.position()`
    // exactly the same way this function does.
    // Audit fix (MED): `depth + 1` — the Code body's nested attribute
    // table is one more level of nesting; bounds Code-in-Code self-nesting.
    let attributes = decode_attributes_vec(buf, cp, source, body_offset, depth + 1, NestedTable::Code)?;

    Ok(Attribute::Code(Box::new(CodeAttribute {
        max_stack,
        max_locals,
        code,
        exception_table,
        attributes,
    })))
}

/// Maximum nesting depth for the `decode_annotation` ⇄ `decode_element_value`
/// mutual recursion. An untrusted annotation can nest `@`/`[` element values
/// arbitrarily deep; without a cap that overflows the stack. 256 far exceeds
/// anything a real compiler emits. Canonical value lives in
/// [`crate::limits::MAX_ANNOTATION_DEPTH`].
const MAX_ANNOTATION_DEPTH: usize = crate::limits::MAX_ANNOTATION_DEPTH;

/// Decode a single annotation structure (JVM spec 4.7.16).
fn decode_annotation(buf: &mut ClassFileBuffer<'_>) -> Result<Annotation, ClassReaderError> {
    decode_annotation_depth(buf, 0)
}

/// Depth-tracking implementation of [`decode_annotation`]. `depth` counts
/// the annotation/array nesting so far; it is checked against
/// [`MAX_ANNOTATION_DEPTH`] here and inside [`decode_element_value_depth`].
fn decode_annotation_depth(
    buf: &mut ClassFileBuffer<'_>,
    depth: usize,
) -> Result<Annotation, ClassReaderError> {
    if depth >= MAX_ANNOTATION_DEPTH {
        return Err(ClassReaderError::InvalidClassData {
            message: "annotation nesting depth exceeds limit".to_string(),
        });
    }
    let type_index = buf.read_u16()?;
    let num_element_value_pairs = buf.read_u16()?;
    let mut element_value_pairs =
        Vec::with_capacity((num_element_value_pairs as usize).min(PREALLOC_CAP));
    for _ in 0..num_element_value_pairs {
        let element_name_index = buf.read_u16()?;
        let value = decode_element_value_depth(buf, depth + 1)?;
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

/// Decode an `element_value` structure (JVM spec 4.7.16.1).
fn decode_element_value(buf: &mut ClassFileBuffer<'_>) -> Result<ElementValue, ClassReaderError> {
    decode_element_value_depth(buf, 0)
}

/// Depth-tracking implementation of [`decode_element_value`]. The `[`
/// (array) and `@` (nested annotation) tags recurse; each recursion
/// increments `depth`, and exceeding [`MAX_ANNOTATION_DEPTH`] returns
/// an error instead of descending further.
fn decode_element_value_depth(
    buf: &mut ClassFileBuffer<'_>,
    depth: usize,
) -> Result<ElementValue, ClassReaderError> {
    if depth >= MAX_ANNOTATION_DEPTH {
        return Err(ClassReaderError::InvalidClassData {
            message: "annotation element_value nesting depth exceeds limit".to_string(),
        });
    }
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
            let annotation = decode_annotation_depth(buf, depth + 1)?;
            Ok(ElementValue::AnnotationValue(annotation))
        }
        b'[' => {
            let num_values = buf.read_u16()?;
            let mut values = Vec::with_capacity((num_values as usize).min(PREALLOC_CAP));
            for _ in 0..num_values {
                values.push(decode_element_value_depth(buf, depth + 1)?);
            }
            Ok(ElementValue::Array(values))
        }
        _ => Err(ClassReaderError::InvalidClassData {
            message: format!("invalid element_value tag: 0x{tag:02X} ('{}')", tag as char),
        }),
    }
}

/// Decode a type_annotation structure (JVM spec 4.7.20).
fn decode_type_annotation(
    buf: &mut ClassFileBuffer<'_>,
) -> Result<TypeAnnotation, ClassReaderError> {
    let target_type = buf.read_u8()?;
    let target_info = decode_target_info(buf, target_type)?;
    let type_path = decode_type_path(buf)?;
    let annotation = decode_annotation(buf)?;
    Ok(TypeAnnotation {
        target_type,
        target_info,
        type_path,
        annotation,
    })
}

/// Decode target_info bytes based on target_type (JVM spec Table 4.7.20-A/B).
/// Stored as raw bytes — see the eager-path comment for the full rationale.
fn decode_target_info(
    buf: &mut ClassFileBuffer<'_>,
    target_type: u8,
) -> Result<Vec<u8>, ClassReaderError> {
    match target_type {
        0x00 | 0x01 => {
            let b = buf.read_u8()?;
            Ok(vec![b])
        }
        0x10 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        0x11 | 0x12 => {
            let a = buf.read_u8()?;
            let b = buf.read_u8()?;
            Ok(vec![a, b])
        }
        0x13..=0x15 => Ok(vec![]),
        0x16 => {
            let b = buf.read_u8()?;
            Ok(vec![b])
        }
        0x17 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        0x40 | 0x41 => {
            // Round 7 audit fix (MED #5 / round-4 #5): avoid re-serialising
            // the `table_length` u16 we just decoded. The previous code
            // called `read_u16()` (which advances the buffer past the
            // two length bytes), then *manually pushed those same two
            // bytes back* into the output `Vec` via `>> 8` / `as u8`
            // before extending with the body. That's two needless byte
            // pushes plus the shift/cast arithmetic per type-annotation.
            //
            // Instead, peek the length via a single `read_bytes(2)`
            // (gives us the raw big-endian bytes verbatim), then do one
            // contiguous `read_bytes(byte_count)` for the body and copy
            // header + body into the output in two slice-copies. No
            // bit-shifts, no per-byte pushes.
            let len_bytes = buf.read_bytes(2)?;
            let len_copy: [u8; 2] = [len_bytes[0], len_bytes[1]];
            let table_length = u16::from_be_bytes(len_copy);
            // `checked_span` rather than a bare `6 *`: the product is
            // u16-bounded today, but the multiplication is the exact shape
            // that wraps to a short span if the field ever widens.
            let byte_count = checked_span(
                "type_annotation localvar_target",
                table_length as usize,
                LOCALVAR_TARGET_ENTRY_SIZE,
            )?;
            // Reserve from the bytes we actually got, not from the declared
            // length: `read_bytes` has already proved the input holds them.
            let body = buf.read_bytes(byte_count)?;
            let mut data = Vec::with_capacity(2 + body.len());
            data.extend_from_slice(&len_copy);
            data.extend_from_slice(body);
            Ok(data)
        }
        0x42 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
        0x43..=0x46 => {
            let hi = buf.read_u8()?;
            let lo = buf.read_u8()?;
            Ok(vec![hi, lo])
        }
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

/// Decode a type_path structure (JVM spec 4.7.20.2).
fn decode_type_path(buf: &mut ClassFileBuffer<'_>) -> Result<Vec<TypePathEntry>, ClassReaderError> {
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

/// Canonical attribute name for an already-decoded [`Attribute`] variant.
///
/// Used by [`LazyAttribute::name`] when the lazy attribute is already in
/// `Decoded` form (no stored `name` string). Names match those in the JVM
/// spec §4.7.
pub fn attribute_canonical_name(attr: &Attribute) -> &str {
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
        Attribute::Unknown { name, .. } => name,
    }
}

#[cfg(test)]
mod tests;
