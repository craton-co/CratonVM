// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use crate::class_file_version::{ClassFileVersion, VersionRejection};
use thiserror::Error;

/// Errors that can occur when reading a `.class` file.
#[derive(Debug, Error)]
pub enum ClassReaderError {
    #[error("unexpected end of data at position {position}")]
    UnexpectedEndOfData { position: usize },

    #[error("invalid magic number: expected 0xCAFEBABE, got 0x{magic:08X}")]
    InvalidMagicNumber { magic: u32 },

    /// JVMS 4.1: the `major.minor` pair is not loadable by this runtime.
    ///
    /// The old text (`unsupported class file version {major}.{minor}`) is kept
    /// as a strict prefix so anything grepping logs for it still matches; the
    /// `rejection` clause is appended because the bare version pair is actively
    /// misleading for the preview cases — `69.65535` *is* a version this parser
    /// supports, it is merely gated.
    #[error("unsupported class file version {major}.{minor}: {rejection}")]
    UnsupportedVersion {
        major: u16,
        minor: u16,
        rejection: VersionRejection,
    },

    #[error("invalid constant pool entry: {message} (at index {index})")]
    InvalidConstantPool { index: u16, message: String },

    #[error("invalid constant pool tag: {tag} (at index {index})")]
    InvalidConstantPoolTag { index: u16, tag: u8 },

    #[error("invalid type descriptor: {descriptor}")]
    InvalidTypeDescriptor { descriptor: String },

    #[error("invalid method descriptor: {descriptor}")]
    InvalidMethodDescriptor { descriptor: String },

    #[error("invalid class data: {message}")]
    InvalidClassData { message: String },

    #[error("invalid modified CESU-8 string at constant pool index {index}")]
    InvalidCesu8String { index: u16 },

    #[error("unknown attribute: {name}")]
    UnknownAttribute { name: String },
}

impl ClassReaderError {
    /// HotSpot's `java.lang.UnsupportedClassVersionError` message for this
    /// error, or `None` if this error is not a version rejection.
    ///
    /// The reader cannot build this itself: HotSpot's wording embeds the class
    /// name, and `this_class` is not read until after the constant pool — long
    /// after the version check that must happen first. The caller already holds
    /// the name it asked for, so it supplies it here.
    ///
    /// `internal_name` must be the slash-separated internal form (`pk/C`), which
    /// is what HotSpot prints; measured, see
    /// docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md.
    ///
    /// The exception to throw with it is `java.lang.UnsupportedClassVersionError`,
    /// which is a subclass of `ClassFormatError` — so a caller that keeps
    /// raising `ClassFormatError` is wrong in the type but not in the hierarchy,
    /// and a `catch (ClassFormatError)` still fires.
    pub fn unsupported_class_version_message(&self, internal_name: &str) -> Option<String> {
        match self {
            Self::UnsupportedVersion {
                major,
                minor,
                rejection,
            } => Some(rejection.message(ClassFileVersion::new(*major, *minor), internal_name)),
            _ => None,
        }
    }
}
