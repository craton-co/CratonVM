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

    /// A refusal HotSpot's `ClassFileParser` words itself (interpreter round
    /// i5 wave 109, lane L5; page
    /// `i108-L7-code-attribute-parse-refusals-use-the-readers-words`, probe
    /// `tools/probes/interp/L5/L5W109ClassFileParserTexts.java`). `reader` is
    /// the reader's own error, whose words this variant's `Display` keeps (log
    /// lines and the reader's tests read those); `hotspot` is the
    /// `ClassFormatError` text HotSpot 25 raises for the same bytes, without
    /// its ` in class file <name>` tail; `class` is the class file's
    /// `this_class` once the reader had read it ([`Self::naming_class`]). The
    /// define door builds the Java message through
    /// [`Self::hotspot_class_format_message`].
    #[error("{reader}")]
    ClassFileParser {
        hotspot: String,
        class: Option<std::sync::Arc<str>>,
        reader: Box<ClassReaderError>,
    },
}

/// HotSpot's `ClassFileStream::truncated_file_error` text. It names no class.
pub const TRUNCATED_CLASS_FILE: &str = "Truncated class file";

impl ClassReaderError {
    /// Wrap `reader` as the refusal HotSpot words `hotspot` (see
    /// [`ClassReaderError::ClassFileParser`]).
    pub fn class_file_parser(hotspot: impl Into<String>, reader: ClassReaderError) -> Self {
        Self::ClassFileParser {
            hotspot: hotspot.into(),
            class: None,
            reader: Box::new(reader),
        }
    }

    /// The same error, naming `this_class` when it is a
    /// [`ClassReaderError::ClassFileParser`] that does not name one yet.
    pub fn naming_class(self, this_class: &std::sync::Arc<str>) -> Self {
        match self {
            Self::ClassFileParser {
                hotspot,
                class: None,
                reader,
            } => Self::ClassFileParser {
                hotspot,
                class: Some(this_class.clone()),
                reader,
            },
            other => other,
        }
    }

    /// HotSpot's `java.lang.ClassFormatError` message for this error, or
    /// `None` when the reader has no HotSpot wording for it (the caller keeps
    /// its own). A read past the end of the class file is `Truncated class
    /// file` (no class named); every other text gets HotSpot's ` in class file
    /// <name>` tail, the name being the class file's `this_class` when the
    /// reader got that far, else `requested` (internal form), else
    /// `<Unknown>` -- HotSpot's `_class_name` at the point of each check.
    pub fn hotspot_class_format_message(&self, requested: &str) -> Option<String> {
        match self {
            Self::UnexpectedEndOfData { .. } => Some(TRUNCATED_CLASS_FILE.to_string()),
            Self::ClassFileParser { hotspot, class, .. } => {
                if hotspot == TRUNCATED_CLASS_FILE {
                    return Some(TRUNCATED_CLASS_FILE.to_string());
                }
                let named = match class.as_deref() {
                    Some(c) => c,
                    None if !requested.is_empty() => requested,
                    None => "<Unknown>",
                };
                Some(format!("{hotspot} in class file {named}"))
            }
            _ => None,
        }
    }

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
