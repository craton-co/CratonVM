// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native-method registry, Panama FFI tables, JNI globals and the fd table.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.natives.<field>`.

use crate::native::io::FileDescriptorTable;
use crate::native::registry::{NativeMethodRegistry, StackTraceEntry};

/// Native-method registry, Panama FFI tables, JNI globals and the fd table.
pub struct NativeRealm {
    /// Native method registry (immutable after construction).
    pub native_methods: NativeMethodRegistry,

    /// File descriptor table for I/O operations.
    pub fd_table: FileDescriptorTable,

    /// Off-heap memory allocations for Panama FFI (JEP 454).
    pub native_memory: parking_lot::Mutex<crate::native::ffi::NativeMemoryTable>,

    /// Loaded native libraries for Panama SymbolLookup (JEP 454).
    pub native_libraries: parking_lot::Mutex<Vec<libloading::Library>>,

    /// Upcall table for Panama upcall handles (C calling Java).
    pub upcall_table: parking_lot::Mutex<crate::native::ffi::UpcallTable>,

    /// JNI global reference table — prevents GC of referenced objects.
    pub jni_global_refs: parking_lot::Mutex<crate::native::jni::JniGlobalRefs>,
}
