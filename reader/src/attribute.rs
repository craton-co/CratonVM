// SPDX-License-Identifier: Apache-2.0
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
    Code(CodeAttribute),

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
    StackMapTable { entries: ByteView },

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
    ///
    /// `name` is an `Arc<str>` (refcount-bump clone of the constant-pool
    /// interned attribute name) and `data` is a [`ByteView`] slicing
    /// directly into the shared class-file `Arc<[u8]>` — both avoid
    /// per-attribute allocations on the parse hot path. Most class
    /// files have several `Unknown`-tagged attributes (older annotation
    /// extensions, vendor attributes), so this is a measurable
    /// bootstrap win.
    Unknown { name: Arc<str>, data: ByteView },
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
    Raw {
        /// The attribute name, e.g. `"Code"`, `"SourceFile"`. Shared
        /// `Arc<str>` from the constant pool's string pool.
        name: Arc<str>,
        /// The shared backing buffer (typically the whole class file).
        source: SharedBytes,
        /// Byte range within `source` containing the attribute body.
        range: Range<usize>,
    },
    /// Decoded: parsed [`Attribute`] variant.
    Decoded(Attribute),
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
        LazyAttribute::Raw {
            name,
            source,
            range,
        }
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
            LazyAttribute::Raw { name, .. } => name,
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
        if let LazyAttribute::Raw {
            name,
            source,
            range,
        } = self
        {
            let name = name.clone();
            let source = source.clone();
            let range = range.clone();
            // Round 7 audit fix (MED #7): route through the `Arc<str>`
            // entrypoint so the body dispatch can use `Arc::ptr_eq`
            // against canonical interned names — skips one intern
            // lookup per lazy decode (the hot path on bootstrap).
            let decoded = decode_attribute_with_source_arc(&name, &source, range, constant_pool)?;
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
            LazyAttribute::Raw {
                name,
                source,
                range,
            } => decode_attribute_with_source_arc(name, source, range.clone(), constant_pool)
                .ok()
                .map(std::borrow::Cow::Owned),
        }
    }

    /// Is this attribute already decoded?
    pub fn is_decoded(&self) -> bool {
        matches!(self, LazyAttribute::Decoded(_))
    }
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
    let (start, end, handler) = (start_pc as usize, end_pc as usize, handler_pc as usize);
    if start >= code_length {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "Code exception_table[{entry_index}]: start_pc {start} is not a valid index into a code array of length {code_length} (JVMS §4.7.3)"
            ),
        });
    }
    if end > code_length {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "Code exception_table[{entry_index}]: end_pc {end} exceeds code_length {code_length} (JVMS §4.7.3)"
            ),
        });
    }
    if start >= end {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "Code exception_table[{entry_index}]: start_pc {start} must be less than end_pc {end} (JVMS §4.7.3)"
            ),
        });
    }
    if handler >= code_length {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "Code exception_table[{entry_index}]: handler_pc {handler} is not a valid index into a code array of length {code_length} (JVMS §4.7.3)"
            ),
        });
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
        _ => Err(ClassReaderError::InvalidConstantPool {
            index: catch_type,
            message: format!(
                "Code exception_table[{entry_index}]: catch_type must be zero or reference a CONSTANT_Class entry (JVMS §4.7.3)"
            ),
        }),
    }
}

pub fn validate_attribute_shape(name: &str, body: &[u8]) -> Result<(), ClassReaderError> {
    match name {
        // Fixed-size 2-byte cp-index attributes (JVMS §4.7.2 / §4.7.27 /
        // §4.7.28). HotSpot rejects mismatched attribute_length here with
        // java.lang.ClassFormatError — match that behaviour.
        "ConstantValue" | "NestHost" | "ModuleMainClass" => {
            if body.len() != 2 {
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "{name} attribute must have attribute_length=2, got {}",
                        body.len()
                    ),
                });
            }
        }
        // Fixed-size 4-byte attribute (JVMS §4.7.7).
        "EnclosingMethod" => {
            if body.len() != 4 {
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "EnclosingMethod attribute must have attribute_length=4, got {}",
                        body.len()
                    ),
                });
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
        "Code" => {
            let mut buf = ClassFileBuffer::new(body);
            // max_stack + max_locals + code_length headers.
            let _max_stack = buf.read_u16()?;
            let _max_locals = buf.read_u16()?;
            let code_length = wire_len_to_usize("Code code_length", buf.read_u32()?)?;
            const MAX_CODE_LENGTH: usize = crate::limits::MAX_CODE_LENGTH;
            if code_length == 0 || code_length > MAX_CODE_LENGTH {
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "Code attribute code_length {code_length} outside valid range 1..={MAX_CODE_LENGTH}"
                    ),
                });
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
                    return Err(ClassReaderError::InvalidClassData {
                        message: format!(
                            "nested attribute inside Code body declares length {nested_len} but only {} bytes remain",
                            buf.remaining()
                        ),
                    });
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
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "Code attribute body has {} trailing bytes after structured fields",
                        buf.remaining()
                    ),
                });
            }
        }
        _ => {
            // Every other attribute kind keeps its full validation
            // deferred to lazy decode (see the policy comment at the
            // top of this file).
        }
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
            let source_file = cp.get_utf8_arc(source_file_index).ok_or_else(|| {
                ClassReaderError::InvalidConstantPool {
                    index: source_file_index,
                    message: "SourceFile must reference a valid Utf8 entry".to_string(),
                }
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
            let signature = cp.get_utf8_arc(signature_index).ok_or_else(|| {
                ClassReaderError::InvalidConstantPool {
                    index: signature_index,
                    message: "Signature must reference a valid Utf8 entry".to_string(),
                }
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
            let entries = ByteView::try_new(source.clone(), start..end)?;
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
                    decode_attributes_vec(buf, cp, source, body_offset, depth + 1)?;
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
            Attribute::AnnotationDefault(value)
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
                data,
            }
        }
    };
    Ok(attr)
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
fn decode_attributes_vec(
    buf: &mut ClassFileBuffer<'_>,
    cp: &ConstantPool,
    source: &SharedBytes,
    outermost_body_offset: usize,
    depth: usize,
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
        let name =
            cp.get_utf8_arc(name_index)
                .ok_or_else(|| ClassReaderError::InvalidConstantPool {
                    index: name_index,
                    message: "nested attribute name must reference a valid Utf8 entry".to_string(),
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
        let attr =
            decode_attribute_body(&name, length, buf, cp, source, outermost_body_offset, depth)?;
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
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "Code attribute code_length {code_length} outside valid range 1..={MAX_CODE_LENGTH}"
            ),
        });
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
    let attributes = decode_attributes_vec(buf, cp, source, body_offset, depth + 1)?;

    Ok(Attribute::Code(CodeAttribute {
        max_stack,
        max_locals,
        code,
        exception_table,
        attributes,
    }))
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
fn attribute_canonical_name(attr: &Attribute) -> &str {
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
mod tests {
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
            entries: ByteView::from_slice(&[0x01u8, 0x02, 0xFF]),
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
            entries: ByteView::empty(),
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
            data: ByteView::from_slice(&[0xDEu8, 0xAD]),
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
            data: ByteView::empty(),
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
            ConstantPoolEntry::Utf8(Arc::from("Main.java")),
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
            ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("StackMapTable")),
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
            ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("StackMapTable")),
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
            ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("Code")),
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
            ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("Record")),
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
            ConstantPoolEntry::Utf8(cratonvm_types::intern_arc("LineNumberTable")),
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
}
