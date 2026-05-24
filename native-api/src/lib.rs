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

/// Lightweight `NativeContext` mock available to tests and to other
/// workspace crates that opt in via the `test-mock` feature.
///
/// See `test_mock::MockNativeContext` for the contract — it's the smallest
/// impl that lets trait default methods (`atomic_fetch_add_int`,
/// `set_static_field_by_name`, …) run without standing up a full VM.
#[cfg(any(test, feature = "test-mock"))]
pub mod test_mock;

pub use intrinsic::InterpIntrinsic;
pub use registry::{
    AnnotationData, AnnotationElementValue, DefineClassFull, FieldMetadata, MethodMetadata,
    NativeCallback, NativeContext, NativeMethodRegistry, StackTraceEntry,
};
