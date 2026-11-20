// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::collections::HashMap;
use std::sync::Arc;

use std::borrow::Borrow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::ptr::NonNull;

pub use cratonvm_types::ThinArcStr;

/// The payload of [`ConstantPoolEntry::Utf8`]: an interned `Arc<str>` in 10
/// bytes at 2-byte alignment (zs s2/fp2).
///
/// A `u16` length and the `Arc<str>`'s data pointer. A `CONSTANT_Utf8` is at
/// most 65 535 bytes of modified UTF-8, and its Rust UTF-8 form is never
/// longer (NUL 2 -> 1 byte, a surrogate pair 6 -> 4, a lone surrogate 3 -> 3
/// as U+FFFD), so the reader's strings fit below the marker; a string of
/// 65 535 bytes or more built by hand (a test, a generator) is held behind a
/// box (`len == u16::MAX`). Placed after the enum's one-byte tag at offset 2,
/// it lets [`ConstantPoolEntry`] be 12 bytes, with [`CpLong`] / [`CpDouble`]
/// at 4-byte alignment. Same allocation and strong count as the `Arc<str>`
/// it was made from: [`CpUtf8::to_arc`] is pointer-equal, and equality,
/// hashing, `Debug` and `Display` are the `str`'s.
#[repr(C, packed(2))]
pub struct CpUtf8 {
    len: u16,
    /// `len < u16::MAX`: the data pointer of an `Arc<str>` (`Arc::into_raw`).
    /// `len == u16::MAX`: a `Box<Arc<str>>` (`Box::into_raw`).
    ptr: NonNull<u8>,
}

// SAFETY: a `CpUtf8` is an owned `Arc<str>` (possibly boxed) in another
// encoding; `Arc<str>` is `Send + Sync`.
unsafe impl Send for CpUtf8 {}
// SAFETY: as above.
unsafe impl Sync for CpUtf8 {}

/// The `CpUtf8::len` marker of the boxed form.
const CP_UTF8_SPILLED: u16 = u16::MAX;

impl CpUtf8 {
    /// Take ownership of `arc` without touching its count.
    pub fn from_arc(arc: Arc<str>) -> Self {
        let len = arc.len();
        if len < CP_UTF8_SPILLED as usize {
            let raw = Arc::into_raw(arc) as *const u8 as *mut u8;
            // SAFETY: `Arc::into_raw` never returns null.
            let ptr = unsafe { NonNull::new_unchecked(raw) };
            Self {
                len: len as u16, // Cast: below u16::MAX, checked above
                ptr,
            }
        } else {
            let raw = Box::into_raw(Box::new(arc)) as *mut u8;
            // SAFETY: `Box::into_raw` never returns null.
            let ptr = unsafe { NonNull::new_unchecked(raw) };
            Self {
                len: CP_UTF8_SPILLED,
                ptr,
            }
        }
    }

    /// The `Arc<str>` fat pointer of the inline form; `None` when boxed.
    #[inline]
    fn inline_raw(&self) -> Option<*const str> {
        let len = self.len;
        let ptr = self.ptr;
        (len != CP_UTF8_SPILLED).then(|| {
            std::ptr::slice_from_raw_parts(ptr.as_ptr() as *const u8, len as usize) as *const str
        })
    }

    /// The boxed `Arc<str>` of the spilled form.
    #[inline]
    fn spilled(&self) -> &Arc<str> {
        let ptr = self.ptr;
        // SAFETY: only reached when `len == CP_UTF8_SPILLED`, where `ptr` is a
        // live `Box<Arc<str>>` owned by `self`.
        unsafe { &*(ptr.as_ptr() as *const Arc<str>) }
    }

    /// The string.
    #[inline]
    pub fn as_str(&self) -> &str {
        match self.inline_raw() {
            // SAFETY: the exact fat pointer `Arc::into_raw` returned; `self`
            // holds one strong count, so the bytes live as long as `&self`.
            Some(raw) => unsafe { &*raw },
            None => self.spilled(),
        }
    }

    /// A new `Arc<str>` sharing this string's allocation (a refcount bump),
    /// `Arc::ptr_eq` with the one this was made from.
    #[inline]
    pub fn to_arc(&self) -> Arc<str> {
        match self.inline_raw() {
            // SAFETY: `raw` came from `Arc::into_raw` and `self` keeps one
            // count alive; increment, then adopt the new count.
            Some(raw) => unsafe {
                Arc::increment_strong_count(raw);
                Arc::from_raw(raw)
            },
            None => Arc::clone(self.spilled()),
        }
    }

    /// The string as an `Arc<str>`, consuming `self`.
    pub fn into_arc(self) -> Arc<str> {
        self.to_arc()
    }

    /// `true` when `self` shares `arc`'s allocation.
    #[inline]
    pub fn ptr_eq_arc(&self, arc: &Arc<str>) -> bool {
        std::ptr::eq(self.as_str().as_ptr(), arc.as_ptr())
    }
}

impl Drop for CpUtf8 {
    fn drop(&mut self) {
        match self.inline_raw() {
            // SAFETY: `self` owns one strong count of the `Arc<str>` at `raw`.
            Some(raw) => unsafe { drop(Arc::from_raw(raw)) },
            None => {
                let ptr = self.ptr;
                // SAFETY: the spilled form owns its `Box<Arc<str>>`.
                unsafe { drop(Box::from_raw(ptr.as_ptr() as *mut Arc<str>)) }
            }
        }
    }
}

impl Clone for CpUtf8 {
    #[inline]
    fn clone(&self) -> Self {
        Self::from_arc(self.to_arc())
    }
}

impl Deref for CpUtf8 {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for CpUtf8 {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for CpUtf8 {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<Arc<str>> for CpUtf8 {
    #[inline]
    fn from(arc: Arc<str>) -> Self {
        Self::from_arc(arc)
    }
}

impl From<&Arc<str>> for CpUtf8 {
    #[inline]
    fn from(arc: &Arc<str>) -> Self {
        Self::from_arc(Arc::clone(arc))
    }
}

impl From<&str> for CpUtf8 {
    /// A fresh, un-interned allocation, as `Arc::<str>::from(&str)`.
    #[inline]
    fn from(s: &str) -> Self {
        Self::from_arc(Arc::from(s))
    }
}

impl From<String> for CpUtf8 {
    #[inline]
    fn from(s: String) -> Self {
        Self::from_arc(Arc::from(s))
    }
}

impl From<&String> for CpUtf8 {
    #[inline]
    fn from(s: &String) -> Self {
        Self::from_arc(Arc::from(s.as_str()))
    }
}

impl From<ThinArcStr> for CpUtf8 {
    #[inline]
    fn from(s: ThinArcStr) -> Self {
        Self::from_arc(s.into_arc())
    }
}

impl From<CpUtf8> for Arc<str> {
    #[inline]
    fn from(s: CpUtf8) -> Self {
        s.into_arc()
    }
}

impl From<&CpUtf8> for Arc<str> {
    #[inline]
    fn from(s: &CpUtf8) -> Self {
        s.to_arc()
    }
}

impl From<&CpUtf8> for ThinArcStr {
    #[inline]
    fn from(s: &CpUtf8) -> Self {
        ThinArcStr::from_arc(s.to_arc())
    }
}

impl PartialEq for CpUtf8 {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for CpUtf8 {}

impl PartialEq<str> for CpUtf8 {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CpUtf8 {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl Hash for CpUtf8 {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl fmt::Debug for CpUtf8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for CpUtf8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

/// The payload of [`ConstantPoolEntry::Long`]: an `i64` stored as two `u32`
/// halves (4-byte alignment), so the enum is 12 bytes (zs s2/fp2). Read it
/// with [`CpLong::get`]; `Debug` and `Display` print the `i64`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CpLong([u32; 2]);

impl CpLong {
    #[inline]
    pub const fn new(v: i64) -> Self {
        let b = v as u64; // Cast: bit pattern
        Self([b as u32, (b >> 32) as u32]) // Cast: the two halves
    }

    #[inline]
    pub const fn get(self) -> i64 {
        (((self.0[1] as u64) << 32) | self.0[0] as u64) as i64 // Cast: bit pattern
    }
}

impl From<i64> for CpLong {
    #[inline]
    fn from(v: i64) -> Self {
        Self::new(v)
    }
}

impl fmt::Debug for CpLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.get(), f)
    }
}

impl fmt::Display for CpLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.get(), f)
    }
}

/// The payload of [`ConstantPoolEntry::Double`]: an `f64`'s bits as two
/// `u32` halves (4-byte alignment; zs s2/fp2). [`CpDouble::get`] and
/// [`CpDouble::to_bits`] read it back bit-exactly (NaN payloads included).
#[derive(Clone, Copy)]
pub struct CpDouble(CpLong);

impl CpDouble {
    #[inline]
    pub const fn new(v: f64) -> Self {
        Self(CpLong::new(v.to_bits() as i64)) // Cast: bit pattern
    }

    #[inline]
    pub const fn get(self) -> f64 {
        f64::from_bits(self.to_bits())
    }

    #[inline]
    pub const fn to_bits(self) -> u64 {
        self.0.get() as u64 // Cast: bit pattern
    }
}

impl From<f64> for CpDouble {
    #[inline]
    fn from(v: f64) -> Self {
        Self::new(v)
    }
}

impl fmt::Debug for CpDouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.get(), f)
    }
}

impl fmt::Display for CpDouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.get(), f)
    }
}

/// Represents an entry in the constant pool of a `.class` file.
///
/// The constant pool is a table of structures representing string constants,
/// class and interface names, field names, and other constants referred to
/// within the class file (JVM spec 4.4).
#[derive(Debug, Clone)]
pub enum ConstantPoolEntry {
    /// Placeholder for the unused index 0 and the second slot of Long/Double entries.
    Tombstone,

    /// CONSTANT_Utf8 (tag 1): A UTF-8 encoded string.
    ///
    /// Stored as a shared `Arc<str>` so identical UTF-8 content shares a single
    /// backing allocation across every constant pool that references it. See
    /// [`cratonvm_types::intern_arc`] — the class-reader hot path funnels every
    /// UTF-8 entry through the global string pool so the same class/method
    /// name loaded via many class files occupies memory exactly once.
    ///
    /// zs s2/fp2: held as a [`CpUtf8`] (the same `Arc<str>` allocation and
    /// count, in 10 bytes at 2-byte alignment) rather than the 16-byte fat
    /// `Arc<str>`; with [`CpLong`] / [`CpDouble`] that takes the entry from 24
    /// to 12 bytes a slot
    /// (`zgc-zr11md-constant-pool-slots-are-24-bytes-20261013.md`). The string
    /// derefs to `str`; [`CpUtf8::to_arc`] gives back the pointer-equal
    /// `Arc<str>`.
    Utf8(CpUtf8),

    /// CONSTANT_Integer (tag 3): A 4-byte integer constant.
    Integer(i32),

    /// CONSTANT_Float (tag 4): A 4-byte float constant.
    Float(f32),

    /// CONSTANT_Long (tag 5): An 8-byte long constant. Takes two constant pool slots.
    /// Read with [`CpLong::get`] (4-byte aligned halves; zs s2/fp2).
    Long(CpLong),

    /// CONSTANT_Double (tag 6): An 8-byte double constant. Takes two constant pool slots.
    /// Read with [`CpDouble::get`] (4-byte aligned halves; zs s2/fp2).
    Double(CpDouble),

    /// CONSTANT_Class (tag 7): A symbolic reference to a class or interface.
    ClassReference { name_index: u16 },

    /// CONSTANT_String (tag 8): A constant string value.
    StringReference { string_index: u16 },

    /// CONSTANT_Fieldref (tag 9): A symbolic reference to a field.
    FieldReference {
        class_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_Methodref (tag 10): A symbolic reference to a class method.
    MethodReference {
        class_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_InterfaceMethodref (tag 11): A symbolic reference to an interface method.
    InterfaceMethodReference {
        class_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_NameAndType (tag 12): A field or method name and descriptor.
    NameAndType {
        name_index: u16,
        descriptor_index: u16,
    },

    /// CONSTANT_MethodHandle (tag 15): A method handle (Java 7+).
    MethodHandle {
        reference_kind: u8,
        reference_index: u16,
    },

    /// CONSTANT_MethodType (tag 16): A method type (Java 7+).
    MethodType { descriptor_index: u16 },

    /// CONSTANT_Dynamic (tag 17): A dynamically-computed constant (Java 11+).
    Dynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_InvokeDynamic (tag 18): A dynamically-computed call site (Java 7+).
    InvokeDynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },

    /// CONSTANT_Module (tag 19): A module (Java 9+).
    Module { name_index: u16 },

    /// CONSTANT_Package (tag 20): A package (Java 9+).
    Package { name_index: u16 },
}

/// The constant pool of a class file.
///
/// Uses 1-based indexing as per the JVM specification. Index 0 is always a Tombstone.
/// Long and Double entries occupy two slots (the second is also a Tombstone).
///
/// zs s2/fp2: the slots are an exact-size boxed slice and the rare wide-UTF-16
/// side table is boxed only when present, so the pool's own header (inline in
/// every class) is 24 bytes, not 72.
#[derive(Debug)]
pub struct ConstantPool {
    entries: Box<[ConstantPoolEntry]>,
    /// Exact UTF-16 code units for the rare `CONSTANT_Utf8` entries that contain
    /// **lone surrogates** (U+D800..U+DFFF), which a Rust `str` cannot hold.
    /// Keyed by 1-based constant-pool index. The matching [`ConstantPoolEntry::Utf8`]
    /// stores a lossy (U+FFFD-substituted) string so that name/descriptor
    /// consumers keep working, while string-constant materialisation
    /// (`ldc` / `ConstantValue`) consults this table via [`get_utf8_wide`] to
    /// reproduce the precise `java.lang.String` `char[]`. Empty for the
    /// overwhelming majority of class files (ANTLR-generated lexers/parsers are
    /// the common case that populates it).
    ///
    /// [`get_utf8_wide`]: ConstantPool::get_utf8_wide
    #[expect(
        clippy::box_collection,
        reason = "keep the common pool header at 24 bytes"
    )]
    wide_utf8: Option<Box<HashMap<u16, Arc<[u16]>>>>,
}

impl ConstantPool {
    pub fn new(entries: Vec<ConstantPoolEntry>) -> Self {
        Self {
            entries: entries.into_boxed_slice(),
            wide_utf8: None,
        }
    }

    /// Construct a constant pool that carries a side table of exact UTF-16 code
    /// units for surrogate-bearing `CONSTANT_Utf8` entries. See [`wide_utf8`].
    ///
    /// [`wide_utf8`]: ConstantPool::wide_utf8
    pub fn new_with_wide(
        entries: Vec<ConstantPoolEntry>,
        wide_utf8: HashMap<u16, Arc<[u16]>>,
    ) -> Self {
        Self {
            entries: entries.into_boxed_slice(),
            wide_utf8: (!wide_utf8.is_empty()).then(|| Box::new(wide_utf8)),
        }
    }

    /// Exact UTF-16 code units for a `CONSTANT_Utf8` entry that contained lone
    /// surrogates, or `None` for ordinary entries (whose `Arc<str>` UTF-8 form
    /// is lossless — use [`get_utf8`]). The returned units include any lone
    /// surrogates verbatim, so callers materialising a `java.lang.String`
    /// reproduce the original `char[]` byte-for-byte.
    ///
    /// [`get_utf8`]: ConstantPool::get_utf8
    pub fn get_utf8_wide(&self, index: u16) -> Option<&[u16]> {
        self.wide_utf8.as_ref()?.get(&index).map(|a| a.as_ref())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() <= 1
    }

    /// Get an entry by 1-based index.
    pub fn get(&self, index: u16) -> Option<&ConstantPoolEntry> {
        self.entries.get(index as usize)
    }

    /// Get a UTF-8 string by 1-based index.
    pub fn get_utf8(&self, index: u16) -> Option<&str> {
        match self.get(index) {
            Some(ConstantPoolEntry::Utf8(s)) => Some(s.as_ref()),
            _ => None,
        }
    }

    /// Get a UTF-8 string by 1-based index as a shared `Arc<str>`.
    ///
    /// Returns a clone of the interned backing `Arc<str>` — callers that need
    /// to hold onto the string beyond the lifetime of the constant pool
    /// should use this instead of [`get_utf8`]. No re-allocation occurs; the
    /// clone is a single refcount bump. Originates from
    /// [`cratonvm_types::intern_arc`] at parse time.
    pub fn get_utf8_arc(&self, index: u16) -> Option<Arc<str>> {
        match self.get(index) {
            Some(ConstantPoolEntry::Utf8(s)) => Some(s.to_arc()),
            _ => None,
        }
    }

    /// Get a class name by resolving a ClassReference at the given index.
    pub fn get_class_name(&self, index: u16) -> Option<&str> {
        match self.get(index) {
            Some(ConstantPoolEntry::ClassReference { name_index }) => self.get_utf8(*name_index),
            _ => None,
        }
    }

    /// Get a class name by resolving a ClassReference, as a shared `Arc<str>`.
    ///
    /// Same as [`get_class_name`] but returns the interned `Arc<str>` directly
    /// (a refcount bump rather than an allocation). Hot path for the bytecode
    /// verifier, which materialises `VType::ObjectRef(Arc<str>)` for every
    /// `new` / `checkcast` / exception-handler catch type.
    pub fn get_class_name_arc(&self, index: u16) -> Option<Arc<str>> {
        match self.get(index) {
            Some(ConstantPoolEntry::ClassReference { name_index }) => {
                self.get_utf8_arc(*name_index)
            }
            _ => None,
        }
    }

    /// Get the name and descriptor from a NameAndType entry.
    pub fn get_name_and_type(&self, index: u16) -> Option<(&str, &str)> {
        match self.get(index) {
            Some(ConstantPoolEntry::NameAndType {
                name_index,
                descriptor_index,
            }) => {
                let name = self.get_utf8(*name_index)?;
                let descriptor = self.get_utf8(*descriptor_index)?;
                Some((name, descriptor))
            }
            _ => None,
        }
    }

    /// Get the name and descriptor from a NameAndType entry as shared `Arc<str>`s.
    pub fn get_name_and_type_arc(&self, index: u16) -> Option<(Arc<str>, Arc<str>)> {
        match self.get(index) {
            Some(ConstantPoolEntry::NameAndType {
                name_index,
                descriptor_index,
            }) => {
                let name = self.get_utf8_arc(*name_index)?;
                let descriptor = self.get_utf8_arc(*descriptor_index)?;
                Some((name, descriptor))
            }
            _ => None,
        }
    }

    /// Validate that all cross-references in the constant pool point to entries
    /// of the correct type. Returns a list of validation errors.
    ///
    /// This catches malformed class files where, e.g., a ClassReference's
    /// `name_index` doesn't point to a Utf8 entry.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let len = self.entries.len() as u16;

        for (i, entry) in self.entries.iter().enumerate() {
            match entry {
                ConstantPoolEntry::ClassReference { name_index } => {
                    if *name_index == 0 || *name_index >= len {
                        errors.push(format!(
                            "cp#{i}: ClassReference name_index {} out of bounds",
                            name_index
                        ));
                    } else if !matches!(
                        self.entries.get(*name_index as usize),
                        Some(ConstantPoolEntry::Utf8(_))
                    ) {
                        errors.push(format!(
                            "cp#{i}: ClassReference name_index {} does not point to Utf8",
                            name_index
                        ));
                    }
                }
                ConstantPoolEntry::StringReference { string_index } => {
                    if *string_index == 0 || *string_index >= len {
                        errors.push(format!(
                            "cp#{i}: StringReference string_index {} out of bounds",
                            string_index
                        ));
                    } else if !matches!(
                        self.entries.get(*string_index as usize),
                        Some(ConstantPoolEntry::Utf8(_))
                    ) {
                        errors.push(format!(
                            "cp#{i}: StringReference string_index {} does not point to Utf8",
                            string_index
                        ));
                    }
                }
                ConstantPoolEntry::FieldReference {
                    class_index,
                    name_and_type_index,
                }
                | ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                }
                | ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                } => {
                    if *class_index == 0 || *class_index >= len {
                        errors.push(format!("cp#{i}: class_index {} out of bounds", class_index));
                    } else if !matches!(
                        self.entries.get(*class_index as usize),
                        Some(ConstantPoolEntry::ClassReference { .. })
                    ) {
                        errors.push(format!(
                            "cp#{i}: class_index {} does not point to ClassReference",
                            class_index
                        ));
                    }
                    if *name_and_type_index == 0 || *name_and_type_index >= len {
                        errors.push(format!(
                            "cp#{i}: name_and_type_index {} out of bounds",
                            name_and_type_index
                        ));
                    } else if !matches!(
                        self.entries.get(*name_and_type_index as usize),
                        Some(ConstantPoolEntry::NameAndType { .. })
                    ) {
                        errors.push(format!(
                            "cp#{i}: name_and_type_index {} does not point to NameAndType",
                            name_and_type_index
                        ));
                    }
                }
                ConstantPoolEntry::NameAndType {
                    name_index,
                    descriptor_index,
                } => {
                    if *name_index == 0 || *name_index >= len {
                        errors.push(format!(
                            "cp#{i}: NameAndType name_index {} out of bounds",
                            name_index
                        ));
                    } else if !matches!(
                        self.entries.get(*name_index as usize),
                        Some(ConstantPoolEntry::Utf8(_))
                    ) {
                        errors.push(format!(
                            "cp#{i}: NameAndType name_index {} does not point to Utf8",
                            name_index
                        ));
                    }
                    if *descriptor_index == 0 || *descriptor_index >= len {
                        errors.push(format!(
                            "cp#{i}: NameAndType descriptor_index {} out of bounds",
                            descriptor_index
                        ));
                    } else if !matches!(
                        self.entries.get(*descriptor_index as usize),
                        Some(ConstantPoolEntry::Utf8(_))
                    ) {
                        errors.push(format!(
                            "cp#{i}: NameAndType descriptor_index {} does not point to Utf8",
                            descriptor_index
                        ));
                    }
                }
                ConstantPoolEntry::MethodType { descriptor_index } => {
                    if *descriptor_index == 0 || *descriptor_index >= len {
                        errors.push(format!(
                            "cp#{i}: MethodType descriptor_index {} out of bounds",
                            descriptor_index
                        ));
                    } else if !matches!(
                        self.entries.get(*descriptor_index as usize),
                        Some(ConstantPoolEntry::Utf8(_))
                    ) {
                        errors.push(format!(
                            "cp#{i}: MethodType descriptor_index {} does not point to Utf8",
                            descriptor_index
                        ));
                    }
                }
                // Audit fix (LOW): the `_ => {}` arm previously skipped
                // MethodHandle / Dynamic / InvokeDynamic, leaving their
                // cross-references unvalidated. Extend the walk to cover
                // them. (`bootstrap_method_attr_index` on Dynamic /
                // InvokeDynamic indexes the BootstrapMethods *attribute*,
                // not the constant pool, so it is intentionally not checked
                // here — only the constant-pool-relative
                // `name_and_type_index` is.)
                ConstantPoolEntry::MethodHandle {
                    reference_index, ..
                } => {
                    // JVMS §4.4.8: reference_index must point to a Fieldref,
                    // Methodref, or InterfaceMethodref. The exact kind is
                    // determined by reference_kind (and is version-dependent
                    // for kinds 6/7), so — matching the conservative,
                    // entry-type-only style of the checks above — we verify
                    // only that the target is one of those three reference
                    // kinds, not the precise kind→reference mapping.
                    if *reference_index == 0 || *reference_index >= len {
                        errors.push(format!(
                            "cp#{i}: MethodHandle reference_index {} out of bounds",
                            reference_index
                        ));
                    } else if !matches!(
                        self.entries.get(*reference_index as usize),
                        Some(
                            ConstantPoolEntry::FieldReference { .. }
                                | ConstantPoolEntry::MethodReference { .. }
                                | ConstantPoolEntry::InterfaceMethodReference { .. }
                        )
                    ) {
                        errors.push(format!(
                            "cp#{i}: MethodHandle reference_index {} does not point to a Field/Method/InterfaceMethod reference",
                            reference_index
                        ));
                    }
                }
                ConstantPoolEntry::Dynamic {
                    name_and_type_index,
                    ..
                }
                | ConstantPoolEntry::InvokeDynamic {
                    name_and_type_index,
                    ..
                } => {
                    // JVMS §4.4.10 / §4.4.11: name_and_type_index must point
                    // to a NameAndType entry.
                    if *name_and_type_index == 0 || *name_and_type_index >= len {
                        errors.push(format!(
                            "cp#{i}: name_and_type_index {} out of bounds",
                            name_and_type_index
                        ));
                    } else if !matches!(
                        self.entries.get(*name_and_type_index as usize),
                        Some(ConstantPoolEntry::NameAndType { .. })
                    ) {
                        errors.push(format!(
                            "cp#{i}: name_and_type_index {} does not point to NameAndType",
                            name_and_type_index
                        ));
                    }
                }
                // C2 hardening: `Module` / `Package` were the last two
                // reference-bearing tags still falling into the catch-all
                // arm, so their `name_index` was the only cross-reference
                // in the pool that `validate` never looked at. JVMS §4.4.11
                // (`CONSTANT_Module_info`) and §4.4.12
                // (`CONSTANT_Package_info`) both require `name_index` to be
                // a valid index to a `CONSTANT_Utf8_info`.
                ConstantPoolEntry::Module { name_index }
                | ConstantPoolEntry::Package { name_index } => {
                    let kind = if matches!(entry, ConstantPoolEntry::Module { .. }) {
                        "Module"
                    } else {
                        "Package"
                    };
                    if *name_index == 0 || *name_index >= len {
                        errors.push(format!(
                            "cp#{i}: {kind} name_index {} out of bounds",
                            name_index
                        ));
                    } else if !matches!(
                        self.entries.get(*name_index as usize),
                        Some(ConstantPoolEntry::Utf8(_))
                    ) {
                        errors.push(format!(
                            "cp#{i}: {kind} name_index {} does not point to Utf8",
                            name_index
                        ));
                    }
                }
                _ => {} // Tombstone, Utf8, Integer, Float, Long, Double
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests;
