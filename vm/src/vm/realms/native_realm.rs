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
    /// `native/jni.rs` until 2026-08-06 — `JDK-ONLY-WAVE2` §6 (retired record:
    /// additional-wave2-markers-not-in-the-original-inventory.md).
    /// Contract §2 forbids process globals for this feature's state, and
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

    /// Dispatches served by [`jni_native_methods`](Self::jni_native_methods)
    /// this run — the §4 census half that moving the table off a process global
    /// did **not** close.
    ///
    /// `NativeMethodRegistry::record_invocation` keys on a `NativeMethodId`, and
    /// only that registry issues one; there is no honest way to mint one for a
    /// `dlsym` result, so these dispatches could not be counted where every
    /// other native dispatch is counted. The consequence was that
    /// `bridge_invocations` silently under-reported genuine JNI bridges by
    /// exactly the number of dispatches through the second table — a
    /// completeness gap in a number whose whole job is to be complete.
    ///
    /// This counter closes it without inventing an id: the report adds this to
    /// the registry's own `Bridge` total, because a `RegisterNatives` /
    /// `dlsym` target **is** a bridge in §1's sense — a real function in a real
    /// library, the case §11 sanctions. `synthetic_stub_invocations` is
    /// untouched and stays exact: a `NativeKind::SyntheticStub` cannot be
    /// created here at all.
    ///
    /// Per-VM for the same reason the table is: a sibling VM's JNI traffic must
    /// not appear in this VM's report.
    pub jni_bridge_invocations: std::sync::atomic::AtomicU64,

    /// Per-triple memo of the strict-mode admission verdict for the JIT's
    /// exact-receiver `java/util/regex/Matcher` leaf, indexed by
    /// `vm/src/jit/helpers.rs`'s `matcher_site_index`.
    ///
    /// Encoding matches `NativeCallSite`'s: `(registry generation << 32) |
    /// verdict`, where verdict `1` = refused and `2` = admitted, and an all-zero
    /// word means "never asked". Keying on the generation makes a verdict taken
    /// before a late `register_*` pass self-heal, which is the same hazard
    /// `NativeCallSite` documents for a bare `OnceLock`.
    ///
    /// Only the strict arm reads it. `Compatible`'s admission is a policy field
    /// read and a `Some`, so memoizing it would cost more than it saves; strict
    /// admission calls `jit_fast_native_has_bytecode`, which takes a
    /// class-manager read lock and walks the hierarchy, and that is what may not
    /// sit on a leaf documented as a 19x fast path.
    ///
    /// Per-VM, not a `static`: the verdict depends on this VM's policy AND on
    /// whether this VM's class manager has concrete bytecode for the triple.
    /// Contract §2 forbids a process global for exactly that kind of state.
    pub matcher_leaf_admission: [std::sync::atomic::AtomicU64; 8],

    /// Set once Netty's `netty_tcnative` shared library has been loaded AND its
    /// `JNI_OnLoad` has run, i.e. the real BoringSSL/OpenSSL binding is live in
    /// this VM.
    ///
    /// Two dispatch decisions read it, and both must flip together or the
    /// package ends up half-real:
    ///
    /// * `vm_exec`'s `skip_jni_incompatible_host_lib` stops refusing
    ///   `io/netty/internal/tcnative/**` symbol resolution, so the
    ///   `RegisterNatives` pointers the library just published are reachable;
    /// * the `SyntheticStub` registrations from
    ///   `register_netty_internal_tcnative_natives` step aside, because the
    ///   registry arm is checked BEFORE the JNI arm — leaving them in place
    ///   would serve `SSL.initialize`/`SSL.version`/every
    ///   `NativeStaticallyReferencedJniMethods` constant from the stub table
    ///   while the rest of the package ran against real BoringSSL state.
    ///
    /// Per-VM rather than a `static` for the same reason `jni_native_methods`
    /// is (Contract §2): a sibling VM in this process may not have loaded the
    /// library at all, and must keep getting the stubs.
    pub netty_tcnative_real: std::sync::atomic::AtomicBool,
}

/// A loaded JNI library outlives the VM that loaded it — deliberately.
///
/// `libloading::Library`'s `Drop` is a `dlclose`/`FreeLibrary`, and dropping the
/// realm would issue one for every library still in the vector. That is not
/// merely wasteful, it is unsound in the same way the explicit-unload path
/// already documents (`NativeSystemAccess::unload_native_library` tombstones an
/// index rather than dropping the handle): code the process is still going to
/// execute lives in that mapping.
///
/// The concrete failure is a `pthread` thread-specific-data destructor.
/// BoringSSL and APR — reached through `netty_tcnative` — register their
/// per-thread cleanup with `pthread_key_create`, and glibc runs those
/// destructors in `__nptl_deallocate_tsd` as the thread *finishes exiting*,
/// which is after `run()` has returned and the realm has been dropped. With the
/// library unmapped, the destructor address is dangling and the VM took a
/// SIGSEGV at `0x…` in the unmapped range on every OpenSSL-touching netty class
/// — after `@@RESULT` had already been printed, so it read as a mystery
/// post-run crash rather than a teardown bug.
///
/// HotSpot has the same rule: a JNI library, once loaded, is never `dlclose`d
/// for the life of the process. Leaking the handle is the fix, not a workaround.
impl Drop for NativeRealm {
    fn drop(&mut self) {
        for lib in self.native_libraries.get_mut().drain(..) {
            std::mem::forget(lib);
        }
    }
}
