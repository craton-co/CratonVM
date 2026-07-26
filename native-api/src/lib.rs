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
pub mod native_id;
pub mod native_ring;
pub mod plain_server_socket_bind;
pub mod plain_server_socket_close;
pub mod registry;
pub mod server_socket_ports;
pub mod socket_input_stream_read;

/// Lightweight `NativeContext` mock available to tests and to other
/// workspace crates that opt in via the `test-mock` feature.
///
/// See `test_mock::MockNativeContext` for the contract — it's the smallest
/// impl that lets trait default methods (`atomic_fetch_add_int`,
/// `set_static_field_by_name`, …) run without standing up a full VM.
#[cfg(any(test, feature = "test-mock"))]
pub mod test_mock;

pub use intrinsic::InterpIntrinsic;
/// Native-dispatch call-site memoization: resolve once, then index.
///
/// `NativeMethodRegistry::find` hashes all three of class/method/descriptor on
/// every call. `NativeCallSite` turns the steady-state cost into an atomic load
/// plus an array index; `NativeMethodKey` removes the hash from the sites that
/// still have to resolve by name. See `native_id` for the full rationale.
pub use native_id::{NativeCallSite, NativeMethodId, NativeMethodKey};
pub use registry::{
    dispatch_baos_event, install_baos_event_hook, AnnotationData, AnnotationElementValue,
    BaosEvent, BaosEventHook, DefineClassFull, FieldMetadata, LambdaSerialMetadata, MethodMetadata,
    NativeCallback, NativeContext, NativeHandle, NativeHandleScope, NativeKind,
    NativeMethodRegistry, NativeThreadBlocker, StackTraceEntry, ThreadJmxSnapshot,
    TypeArgAnnotations,
};
