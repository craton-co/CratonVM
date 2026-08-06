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

    /// JNI native function pointers bound by `RegisterNatives` or by JNI-name
    /// symbol lookup. Key = FNV-1a of `"class.methodDescriptor"`, value = the
    /// raw `fn` address inside the host library.
    ///
    /// This was a `static JNI_NATIVE_METHODS` process global in
    /// `native/jni.rs` until 2026-08-06 — the `JDK-ONLY-WAVE2` item in
    /// `docs/known-issues/jdk-only/additional-wave2-markers-not-in-the-original-inventory.md`
    /// §6. Contract §2 forbids process globals for this feature's state, and
    /// the concrete hazard is the one this repo keeps re-learning: two VMs in
    /// one process saw each other's `RegisterNatives`, so a library loaded by
    /// VM A bound its function pointers for VM B as well. It sits here rather
    /// than in a new realm because `jni_global_refs` — the other per-VM JNI
    /// table — already does.
    ///
    /// It holds only `dlsym` results, so there is nothing to classify: a
    /// `NativeKind::SyntheticStub` cannot be created here, which is why this
    /// second registry is not a §1.3 bypass. See `native/jni.rs`'s
    /// `register_jni_native` for the rest of that argument.
    pub jni_native_methods: parking_lot::RwLock<std::collections::HashMap<u64, usize>>,
}
