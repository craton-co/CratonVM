// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use thiserror::Error;

/// Errors that can occur when reading a `.class` file.
#[derive(Debug, Error)]
pub enum ClassReaderError {
    #[error("unexpected end of data at position {position}")]
    UnexpectedEndOfData { position: usize },

    #[error("invalid magic number: expected 0xCAFEBABE, got 0x{magic:08X}")]
    InvalidMagicNumber { magic: u32 },

    #[error("unsupported class file version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },

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
