// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// TODO: Re-enable missing_docs once the public API stabilises and doc comments
// are added to all spec-derived types (struct fields, enum variants, etc.).
#![allow(missing_docs)]
// Pre-existing style/judgment clippy lints in this spec-derived parser; allowed
// crate-wide so `clippy -D warnings` is green (reader is a build dep of gc, so
// these otherwise block the gc clippy gate too). Not defects.
#![allow(
    clippy::doc_lazy_continuation,
    clippy::explicit_auto_deref,
    clippy::identity_op
)]

//! Java `.class` file parser for the CratonVM project.
//!
//! This crate handles parsing of Java class files according to the JVM specification,
//! supporting class file versions from Java 1.1 (major version 45) through Java 25
//! (major version 69).
//!
//! # Modules
//!
//! - [`class_reader`] — Main entry point for parsing `.class` file bytes
//! - [`constant_pool`] — JVM constant pool entries (all 20 tag types)
//! - [`instruction`] — Bytecode instruction decoding (200+ opcodes)
//! - [`attribute`] — Class/method/field attributes (30+ types)
//! - [`stack_map`] — StackMapTable verification frames (Java 7+)
//! - [`field_type`] / [`method_descriptor`] — JVM type descriptor parsing
//! - [`limits`] — resource limits for untrusted input, and the checked
//!   arithmetic helpers that enforce them

pub mod attribute;
pub mod buffer;
pub mod byte_view;
pub mod class_access_flags;
pub mod class_file;
pub mod class_file_version;
pub mod class_reader;
pub mod class_reader_error;
pub mod constant_pool;
pub mod field;
pub mod field_type;
pub mod instruction;
pub mod jimage;
pub mod limits;
pub mod method;
pub mod method_descriptor;
pub mod quickened;
pub mod signature;
pub mod stack_map;
pub mod verified_code;

pub use attribute::{decode_attribute, force_decode_all, LazyAttribute};
pub use byte_view::{ByteView, SharedBytes};
pub use class_file::ClassFile;
// The preview seam. `set_preview_enabled` is the whole of the reader's half of
// `--enable-preview`: the launcher calls it once with the parsed argument and
// every `read_class*` entry point picks the bit up from there.
pub use class_file_version::{preview_enabled, set_preview_enabled, VersionRejection};
pub use class_reader::{read_class, read_class_arc, read_class_shared};
pub use class_reader_error::ClassReaderError;
pub use constant_pool::ConstantPool;
pub use jimage::{JImageError, JImageReader};
pub use quickened::{intern as intern_quickened, quicken_stats, QuickenedCode};
pub use verified_code::{verified_code, VerifiedCode, VerifiedInstruction};
