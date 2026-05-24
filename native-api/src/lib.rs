// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native method API for CratonVM.
//!
//! Provides the NativeContext trait, NativeMethodRegistry, FFI types,
//! and FileDescriptorTable used by all native method crates.

pub mod charset;
pub mod fd_table;
pub mod ffi;
pub mod init_level;
pub mod intrinsic;
pub mod native_ring;
pub mod registry;

pub use intrinsic::InterpIntrinsic;
pub use registry::{
    AnnotationData, AnnotationElementValue, DefineClassFull, FieldMetadata, MethodMetadata,
    NativeCallback, NativeContext, NativeMethodRegistry, StackTraceEntry,
};
