// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JNI (Java Native Interface) implementation.
//!
//! Provides the standard C API for native code to interact with the JVM.
//! The function table is represented as a flat `[usize; 229]` array matching
//! the JNI spec layout. Native code accesses it via `(*env)[index](env, ...)`.
//!
//! JNI functions access the VM via thread-local storage. Before calling into
//! native code, the interpreter installs the TLS context with `replace_jni_context()`,
//! and clears it on return with `clear_jni_context()`.

#![allow(dead_code)]

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::{Arc, Weak};

use crate::classloading::{find_field_recursive, find_method_recursive, ClassId};
use crate::memory::heap::ArrayElementType;
use crate::threading::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};
use crate::vm::{invoke_on_class_shared, read_java_string, SharedVm};

// ---------------------------------------------------------------------------
// JNI type aliases
//
// These are bare `u64` aliases rather than newtypes for C ABI compatibility
// across hundreds of `extern "C"` call sites.  They represent distinct JNI
// handle types at the conceptual level:
//
//   JObject     – opaque handle to a Java object (0 ≡ null)
//   JClass      – handle to a `java.lang.Class` instance
//   JString     – handle to a `java.lang.String` instance
//   JArray      – handle to a Java array
//   JThrowable  – handle to a `java.lang.Throwable` instance
//   JMethodID   – opaque identifier for a resolved method
//   JFieldID    – opaque identifier for a resolved field
// ---------------------------------------------------------------------------

pub type JBoolean = u8;
pub type JByte = i8;
pub type JChar = u16;
pub type JShort = i16;
pub type JInt = i32;
pub type JLong = i64;
pub type JFloat = f32;
pub type JDouble = f64;
pub type JSize = i32;

/// Opaque handle to a Java object. A value of `0` represents JNI `NULL`.
pub type JObject = u64;
/// Alias for [`JObject`] — handle to a `java.lang.Class`.
pub type JClass = JObject;
/// Alias for [`JObject`] — handle to a `java.lang.String`.
pub type JString = JObject;
/// Alias for [`JObject`] — handle to a Java array.
pub type JArray = JObject;
/// Alias for [`JObject`] — handle to a `java.lang.Throwable`.
pub type JThrowable = JObject;
/// Opaque method identifier obtained from `GetMethodID`/`GetStaticMethodID`.
pub type JMethodID = u64;
/// Opaque field identifier obtained from `GetFieldID`/`GetStaticFieldID`.
pub type JFieldID = u64;

/// JavaVM is a pointer to a pointer to the invocation function table.
pub type JavaVM = *const *const usize;

/// JNIEnv is a pointer to a pointer to the function table (per JNI spec).
pub type JNIEnv = *const *const usize;

pub const JNI_VERSION_1_8: JInt = 0x00010008;
pub const JNI_OK: JInt = 0;
pub const JNI_ERR: JInt = -1;
/// `JNI_EVERSION`: `GetEnv` for a version (interface) the VM does not provide.
pub const JNI_EVERSION: JInt = -3;
/// The interface bits of a `GetEnv` version: 0 for JNI, `0x30000000` for
/// JVMTI (`JVMTI_VERSION_INTERFACE_JVMTI`), `0x10000000` / `0x20000000` for
/// the retired JVMPI / JVMDI.
pub const JNI_VERSION_INTERFACE_MASK: JInt = 0x7000_0000;
pub const JNI_FALSE: JBoolean = 0;
pub const JNI_TRUE: JBoolean = 1;
/// Number of slots in the `JNIEnv` function table.
///
/// This is a property of the JNI HEADER a native was compiled against, not a
/// choice: `(*env)->IsVirtualThread(env, o)` compiles to a load from slot 234,
/// and a table with 234 entries makes that load read one word PAST the
/// allocation. JDK 25's `jni.h` ends at slot 235
/// (`GetStringUTFLengthAsLong`), so the table is sized to 236 and every slot
/// through the end of the header is wired or holds `jni_stub`.
pub const JNI_FUNCTION_COUNT: usize = 236;
pub const JNI_INVOKE_FUNCTION_COUNT: usize = 8;

// ---------------------------------------------------------------------------
// Descriptor cache (bounded LRU)
// ---------------------------------------------------------------------------

/// Maximum number of parsed descriptors retained in the thread-local cache.
const DESCRIPTOR_CACHE_CAPACITY: usize = 1024;

/// A small bounded LRU cache for parsed method descriptors.
///
/// Previously this was a plain `HashMap` capped at [`DESCRIPTOR_CACHE_CAPACITY`]
/// that was `clear()`ed wholesale once full, which caused a periodic full-flush:
/// every entry — including hot, frequently-reused descriptors — was discarded at
/// once, re-incurring a parse storm. This LRU instead evicts only a single
/// (least-recently-used) entry when at capacity, so hot descriptors survive.
///
/// Recency is tracked with a monotonic tick stamped on each access. The eviction
/// scan is O(n) but runs only on insertion-while-full, not on every lookup.
struct DescriptorCache {
    /// descriptor string → (parsed param-type tags, last-access tick)
    map: HashMap<String, (Vec<u8>, u64)>,
    /// Monotonically increasing access counter; larger = more recently used.
    tick: u64,
}

impl DescriptorCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
        }
    }

    /// Look up `descriptor`, marking it most-recently-used on a hit.
    fn get(&mut self, descriptor: &str) -> Option<&[u8]> {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        match self.map.get_mut(descriptor) {
            Some(entry) => {
                entry.1 = tick;
                Some(&entry.0)
            }
            None => None,
        }
    }

    /// Insert `descriptor → types`, evicting the single least-recently-used
    /// entry first if the cache is at capacity.
    fn insert(&mut self, descriptor: &str, types: Vec<u8>) {
        if self.map.len() >= DESCRIPTOR_CACHE_CAPACITY && !self.map.contains_key(descriptor) {
            // Evict exactly one entry: the least-recently-used (smallest tick).
            if let Some(lru_key) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&lru_key);
            }
        }
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        self.map.insert(descriptor.to_owned(), (types, tick));
    }
}

// ---------------------------------------------------------------------------
// Thread-Local VM Context
// ---------------------------------------------------------------------------

/// Per-outstanding-buffer bookkeeping for `Get<Type>ArrayElements`. Recorded at
/// Get, consumed at the matching `Release<Type>ArrayElements`, in the VM's
/// `JniGlobalRefs::elem_copies` (per VM since gc-common w10-c, so a release on
/// any thread finds it).
///
/// `len`/`cap` are the EXACT `Vec` layout we allocated so Release can copy back
/// and free soundly. BUG FIX (vm-jni-roots #2): `Release` used to RE-DERIVE the
/// length from the array handle, and a length that differed from Get's (a
/// moved array, an aliased or stale handle, a 0 read) built
/// `Vec::from_raw_parts` with the wrong layout. `array_gref` is a JNI
/// **global ref** minted for the source array at Get: unlike a raw heap pointer
/// (which a moving GC silently invalidates), a global ref is rewritten by
/// `update_after_gc`/`update_all_roots` after every relocating collection
/// (generational young-copy and G1 evacuation alike), so Release resolves the
/// array's CURRENT address and the copy-back lands in the live array — never a
/// freed/recycled region. It also keeps the array alive across the
/// (spec-permitted, arbitrarily long) Get/Release window.
#[derive(Clone, Copy)]
struct ArrayElemBuffer {
    /// Number of elements actually initialised — bounds the copy-back loop so
    /// we never read an uninitialised tail.
    len: usize,
    /// Original `Vec` capacity — MUST be passed to `Vec::from_raw_parts` for a
    /// sound free.
    cap: usize,
    /// Remappable handle to the source array (a JNI global ref, bit 0 set).
    /// Resolved at Release to the array's post-GC location; deleted on the
    /// final (freeing) Release.
    array_gref: JObject,
}

thread_local! {
    /// Holds an `Arc<SharedVm>` while inside a JNI native call.
    ///
    /// Using `Arc` instead of a raw pointer prevents use-after-free: the
    /// reference count keeps the `SharedVm` alive for the entire duration of the
    /// native call, even if other `Arc` holders drop their references.
    static JNI_SHARED_VM: std::cell::RefCell<Option<Arc<SharedVm>>> =
        std::cell::RefCell::new(None);
    static JNI_THREAD: Cell<*mut ()> = const { Cell::new(std::ptr::null_mut()) };
    static JNI_PENDING_EXCEPTION: Cell<u64> = const { Cell::new(0) };
    /// Generation counter incremented on every set/clear cycle.
    /// Allows detecting stale context in nested native calls.
    static JNI_CONTEXT_GENERATION: Cell<u64> = const { Cell::new(0) };
    // The `GetStringChars` and `Get<Type>ArrayElements` records that lived
    // here (`JNI_STRING_BUFFERS`, `JNI_ARRAY_ELEM_BUFFERS`) are per VM since
    // gc-common w10-c: `JniGlobalRefs::{elem_copies, string_copies}`. A
    // release may come from any thread; a critical section may not, so
    // `JNI_CRITICAL_COPIES` stays here.
    /// Tracks temporary contiguous buffers handed out by
    /// `GetPrimitiveArrayCritical` when the underlying array is a G1
    /// **humongous** array (whose payload is split across non-contiguous
    /// regions and therefore has no flat data pointer). The buffer is a
    /// boxed `Vec<u8>` we materialised by copying every element in; the map
    /// records the metadata `ReleasePrimitiveArrayCritical` needs to copy
    /// the (possibly mutated) bytes back into the array and free the buffer.
    /// Entries are keyed by the returned pointer (`buf.as_mut_ptr() as usize`).
    static JNI_CRITICAL_COPIES: std::cell::RefCell<HashMap<usize, CriticalCopy>> =
        std::cell::RefCell::new(HashMap::new());
    /// Thread-local cache for parsed method descriptors.
    /// Maps descriptor string → parsed parameter type tags, avoiding
    /// repeated parsing of the same descriptor in hot JNI call paths.
    static JNI_DESCRIPTOR_CACHE: std::cell::RefCell<DescriptorCache> =
        std::cell::RefCell::new(DescriptorCache::new());
}

/// Set the JNI thread-local context from an `Arc<SharedVm>` the caller already
/// holds — the Invocation API's entry point, where there is no enclosing native
/// call to preserve.
///
/// **There is deliberately no `set_jni_context` beside this any more.** A plain
/// "install, and clear on the way out" pair is the shape that lost netty's
/// TLSv1.3 client certificate: a native call NESTS, and the inner call's clear
/// took the enclosing call's context with it. [`replace_jni_context`] is what a
/// nesting caller needs, and deleting the non-nesting one — once the fix left it
/// with no production caller — keeps the next one from reintroducing the bug.
/// See [`replace_jni_context`] for the measurement.
pub fn set_jni_context_arc(shared: Arc<SharedVm>) {
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = Some(shared);
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Clear the JNI thread-local context after returning from native code.
/// Drops the `Arc<SharedVm>`, decrementing the reference count.
pub fn clear_jni_context() {
    // `try_with`: this is reached from `DetachCurrentThread`, which a JNI
    // library's `pthread` TSD destructor can call after this thread's TLS is
    // already gone. There is nothing to clear then, and `with` would panic.
    let _ = JNI_SHARED_VM.try_with(|c| {
        *c.borrow_mut() = None;
    });
    let _ = JNI_CONTEXT_GENERATION.try_with(|g| g.set(g.get().wrapping_add(1)));
}

/// Install the JNI context and hand back what was there, so the caller can put
/// it back instead of clearing.
///
/// # The bug this exists to fix
///
/// [`clear_jni_context`] is unconditional, and a native call is not the only
/// thing that can be on this thread's stack. A JNI native that calls **back**
/// into Java, whose Java in turn calls **another** JNI native, produces
///
/// ```text
///   install (outer)  ->  Java upcall  ->  install (inner)  ->  clear (inner)
///                                         ^ outer's context is now gone
/// ```
///
/// and the outer native's *later* upcalls — the ones it makes after the Java
/// callback returns — find `None` in [`with_jni_context`] and are answered as
/// if the VM were not attached. They do not fail loudly; `CallObjectMethodV`
/// and friends return a null `jobject` and the host library reads that as an
/// ordinary answer.
///
/// MEASURED (`probes/OpenSslTls13ReentryBisectProbe.java`, netty +
/// netty-tcnative BoringSSL, TLSv1.3, `useTasks=false`): with BoringSSL's
/// verify callback running Java **inside** `SSL_do_handshake`, a single nested
/// tcnative call from that Java — ANY of them, including
/// `SSL.getLastErrorNumber()` which touches no `SSL*` at all, and
/// `SSL.getOptions()` on an unrelated idle `SSL*` — makes BoringSSL's
/// subsequent CERTIFICATE callback reach no Java. The client's key manager is
/// never asked for an alias (`keyAsks=0` against `1` on every passing row), so
/// the client sends no certificate and the server ends the handshake with
/// `PEER_DID_NOT_RETURN_A_CERTIFICATE`. Java-only, monitor-only, allocation and
/// `System.gc()` arms all pass, which is what rules out every other hypothesis.
///
/// Restoring rather than clearing keeps the outer context alive for exactly as
/// long as the outer call, and still drops the `Arc` at the outermost return —
/// the property [`clear_jni_context`]'s contract is about.
pub fn replace_jni_context(shared: &SharedVm) -> Option<Arc<SharedVm>> {
    let arc = shared.get_arc();
    let prev = JNI_SHARED_VM.with(|c| (*c.borrow_mut()).replace(arc));
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
    prev
}

/// Put back what [`replace_jni_context`] handed out.
///
/// `None` restores the "no context" state, which is what the OUTERMOST native
/// call's exit must leave behind — so this is a strict generalisation of
/// [`clear_jni_context`], not a weakening of it.
pub fn restore_jni_context(prev: Option<Arc<SharedVm>>) {
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = prev;
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Set the JNI thread-local `JvmThread` pointer before entering native code.
///
/// # Safety
///
/// `thread` must point to a valid, exclusively-accessible `JvmThread` for
/// the entire duration of the native call.  The pointer is erased to
/// `*mut ()` in TLS and cast back in `with_jni_thread`, so the caller must
/// guarantee the pointee is not moved, dropped, or concurrently mutated
/// until `clear_jni_thread()` is called.
pub fn set_jni_thread(thread: *mut JvmThread) {
    JNI_THREAD.with(|c| c.set(thread as *mut ()));
}

/// [`set_jni_thread`] that hands back the previous pointer, for the same reason
/// [`replace_jni_context`] exists: a nested native call must not leave the
/// enclosing one without a thread.
///
/// # Safety
///
/// Same contract as [`set_jni_thread`].
pub unsafe fn replace_jni_thread(thread: *mut JvmThread) -> *mut JvmThread {
    JNI_THREAD.with(|c| c.replace(thread as *mut ())) as *mut JvmThread
}

/// Put back what [`replace_jni_thread`] handed out. A null `prev` restores the
/// "no thread" state that [`clear_jni_thread`] leaves.
///
/// # Safety
///
/// `prev` must be a pointer this thread previously had installed, still valid
/// for as long as it stays installed.
pub unsafe fn restore_jni_thread(prev: *mut JvmThread) {
    JNI_THREAD.with(|c| c.set(prev as *mut ()));
}

/// Clear the JNI thread pointer on native code return.
pub fn clear_jni_thread() {
    // `try_with` for the same reason as `clear_jni_context`.
    let _ = JNI_THREAD.try_with(|c| c.set(std::ptr::null_mut()));
}

// ---------------------------------------------------------------------------
// Process-global VM resolution (foreign-thread attach)
// ---------------------------------------------------------------------------
//
// The JNI Invocation API hands `AttachCurrentThread` a `JavaVM*`, but our
// `JavaVM` is an opaque `*const *const usize` — the invocation function table,
// with no back-pointer to the live `SharedVm`. Until that is reachable, an
// attaching foreign thread cannot register itself with the VM (no
// `ThreadRegistry`, no GC-safepoint participation), which is the gap documented
// in `docs/feature-designs/foreign-thread-attach.md`.
//
// There is exactly one VM per process (the `CREATED_VM` singleton in
// `libcratonvm`, mirrored by the leaked `JNI_INVOKE_TABLE_PTR`), and the
// `JavaVM*` we hand back is itself a process-global singleton — so "the
// JavaVM*" and "the one VM" denote the same fact. We therefore publish the
// `Arc<SharedVm>` as a process-global `Weak` cell at VM-create time and let the
// attach path upgrade it. `Weak` (not `Arc`) is deliberate: the cell must not
// keep the VM alive past the owner (`Vm` / `CREATED_VM`).
static PROCESS_VM: parking_lot::Mutex<Option<Weak<SharedVm>>> = parking_lot::Mutex::new(None);

/// Publish the process-global VM so foreign threads can resolve it from a
/// `JavaVM*` in `AttachCurrentThread`. Called once by `JNI_CreateJavaVM`
/// (and `cratonvm_create`) right after the JNI TLS context is set. Idempotent
/// — a later create (e.g. a test that builds a second `Vm` in-process) replaces
/// the cell; the previous `Weak` simply stops upgrading once its `Arc` is gone.
pub fn set_process_vm(shared: &Arc<SharedVm>) {
    // Round 13 wave 1 (lane mhffm patch): FFM upcalls on foreign threads.
    cratonvm_native_builtins::panama_upcall::set_upcall_attach_hook(ffm_upcall_attach);
    let mut cell = PROCESS_VM.lock();
    *cell = Some(Arc::downgrade(shared));
    drop(cell);
    let mut reg = VM_REGISTRY.lock();
    reg.retain(|w| w.strong_count() > 0);
    if !reg.iter().any(|w| w.ptr_eq(&Arc::downgrade(shared))) {
        reg.push(Arc::downgrade(shared));
    }
}

/// Every live VM in the process, not just the most recently published one.
///
/// `PROCESS_VM` is a single cell that `Vm::new` overwrites unconditionally, so
/// on its own it cannot answer "is there more than one VM here?" — and every
/// caller of [`process_vm`] was silently getting whichever VM happened to be
/// created last. This registry exists so that question is answerable, and so
/// the callers that must not guess can refuse instead.
static VM_REGISTRY: parking_lot::Mutex<Vec<Weak<SharedVm>>> = parking_lot::Mutex::new(Vec::new());

/// Number of live VMs in this process.
pub fn live_vm_count() -> usize {
    let mut reg = VM_REGISTRY.lock();
    reg.retain(|w| w.strong_count() > 0);
    reg.len()
}

/// How many times [`process_vm`] answered while more than one VM was live.
///
/// Non-zero means some caller took the most-recently-created VM when the
/// correct one was not determinable. It is a diagnostic, not a gate — see
/// [`process_vm_strict`] for the callers that refuse instead.
pub fn ambiguous_process_vm_resolutions() -> u64 {
    AMBIGUOUS_RESOLUTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

static AMBIGUOUS_RESOLUTIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`process_vm`], but `None` when the answer is ambiguous.
///
/// Use this wherever picking the wrong VM is worse than doing nothing. The JIT
/// safepoint slow path is the motivating case: parking against another VM's
/// stop-the-world barrier is a hang or worse, while declining to park merely
/// leaves this poll ineffective.
pub fn process_vm_strict() -> Option<Arc<SharedVm>> {
    let mut reg = VM_REGISTRY.lock();
    reg.retain(|w| w.strong_count() > 0);
    if reg.len() != 1 {
        return None;
    }
    reg[0].upgrade()
}

/// The live VM whose stop-the-world flag byte lives at `flag_addr`, if any.
///
/// For the JIT safepoint slow path: the poll that fired tested exactly one
/// VM's flag (`JitRuntimeHelpers::safepoint_flag_addr`, built per VM), so this
/// is an identity, not a guess, and it stays correct with several VMs in one
/// process where [`process_vm_strict`] has to refuse. `None` when no live VM
/// owns the address (`0`, or a poll emitted without the argument); the caller
/// then falls back to [`process_vm_strict`].
pub fn vm_for_stw_flag_addr(flag_addr: usize) -> Option<Arc<SharedVm>> {
    if flag_addr == 0 {
        return None;
    }
    // Upgrade outside the lock: a non-matching `Arc` is dropped here, and if
    // it was the last owner its `SharedVm` teardown must not run under
    // `VM_REGISTRY`.
    let candidates: Vec<Weak<SharedVm>> = {
        let mut reg = VM_REGISTRY.lock();
        reg.retain(|w| w.strong_count() > 0);
        reg.clone()
    };
    candidates
        .iter()
        .filter_map(Weak::upgrade)
        .find(|vm| vm.mem.gc_barrier.stw_requested_flag_addr() as usize == flag_addr)
}

/// Resolve the live process-global VM, if one was published and is still alive.
/// Returns an owning `Arc` (keeps the VM alive for the duration of the caller's
/// use) or `None` if no VM was created or it has been dropped.
pub fn process_vm() -> Option<Arc<SharedVm>> {
    let resolved = PROCESS_VM.lock().as_ref().and_then(Weak::upgrade);
    if resolved.is_some() && live_vm_count() > 1 {
        // Answering at all is a guess: this cell holds whichever VM was created
        // LAST, and nothing here knows which one the caller belongs to. Kept
        // rather than made fail-closed because the JNI attach surface has no
        // way to say — a `JavaVM` handle points at one process-global invoke
        // table shared by every VM, which is why `AttachCurrentThread` ignores
        // its own argument. Counted so the guess is visible instead of silent;
        // `process_vm_strict` is the spelling for callers that must not guess.
        AMBIGUOUS_RESOLUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    resolved
}

// ---------------------------------------------------------------------------
// DestroyJavaVM teardown hook
// ---------------------------------------------------------------------------
//
// The `DestroyJavaVM` invocation-table slot (`jni_destroy_java_vm`) is reached
// when a C host calls `(*vm)->DestroyJavaVM(vm)`. The actual VM instance, though,
// is *owned* by the embedding layer (`libcratonvm`'s process-global `CREATED_VM`
// registry), which this crate cannot reach directly. So the embedder registers a
// teardown closure here at create time; `DestroyJavaVM` invokes it. When no hook
// is registered (e.g. a VM built directly via `Vm::new` from Rust, with no
// Invocation-API bootstrap), `DestroyJavaVM` has nothing process-global to drop
// and reports success.

/// Embedder-registered `DestroyJavaVM` teardown. Returns a JNI status code.
static DESTROY_VM_HOOK: parking_lot::Mutex<Option<fn() -> JInt>> = parking_lot::Mutex::new(None);

/// Register the teardown invoked by `DestroyJavaVM`. Called by the embedding
/// layer (`JNI_CreateJavaVM` / `cratonvm_create`) so the slot can drop the VM it
/// owns. Replacing an existing hook is allowed (one VM per process; a later
/// create overwrites). Passing the same fn pointer is idempotent.
pub fn set_destroy_vm_hook(hook: fn() -> JInt) {
    *DESTROY_VM_HOOK.lock() = Some(hook);
}

/// Run the registered teardown hook (if any) and clear it. `None` → `JNI_OK`
/// (nothing process-global to tear down). The hook is taken so a second
/// `DestroyJavaVM` is a no-op success, mirroring HotSpot (the VM is gone).
fn run_destroy_vm_hook() -> JInt {
    let hook = DESTROY_VM_HOOK.lock().take();
    match hook {
        Some(h) => h(),
        None => JNI_OK,
    }
}

// ---------------------------------------------------------------------------
// Foreign-thread attach state (host-created OS threads)
// ---------------------------------------------------------------------------

thread_local! {
    /// Owns the heap-boxed `JvmThread` for a foreign (host-created) OS thread
    /// that attached via `AttachCurrentThread`. The same `JvmThread`'s raw
    /// address is published into `JNI_THREAD` (so `with_jni_context` can reach
    /// it); this box keeps the allocation alive and **address-stable** — the JIT
    /// bakes `tlab_offset`/`shadow_stack_offset`-relative addresses off the live
    /// `JvmThread*` while the thread runs, so it must never be moved or freed
    /// until `DetachCurrentThread`. `None` for VM-created threads (the main
    /// thread parks its `JvmThread` in `Vm`; `Thread.start` workers own theirs
    /// on the spawned stack).
    ///
    /// An OS thread that EXITS attached (no `DetachCurrentThread`, or a detach
    /// that only arrives in the `pthread` TSD phase, after this key is gone)
    /// is reaped by [`ForeignThreadBox`]'s drop (gen r5w1/crash5).
    static FOREIGN_THREAD_BOX: std::cell::RefCell<Option<ForeignThreadBox>> =
        const { std::cell::RefCell::new(None) };
    /// Depth of nested Java calls in flight on a foreign-attached thread. Used
    /// to scope the idle-attached blocked-region transition (§3.3) to the
    /// OUTERMOST Java call: `0` means the thread is idle (between calls, parked
    /// in the host event loop) and is modelled as GC-blocked.
    static FOREIGN_CALL_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// CR-VXC-3 (`vm-exec-closeout.md` §5.3): the
    /// crash handler's publication guard for a foreign-attached thread.
    ///
    /// `crash_handler::java_stack_lines` renders whatever the *faulting* OS
    /// thread published into `vm_util`'s thread-local cell, falling back to the
    /// primordial trace. A JNI-attached host thread that faults while running
    /// Java is exactly the case with no fallback worth printing — the
    /// primordial thread's frames say nothing about it.
    ///
    /// The guard is parked here rather than bound in `attach_foreign_thread`
    /// because it must live for the whole **attachment**, not for the
    /// registration call: a `let _guard = …` in that function would un-publish
    /// the moment it returned, i.e. before the thread ran a single bytecode.
    /// `detach_foreign_thread` takes it back out, which restores whatever the
    /// cell held before the attach (the guard is save-and-restore, so a
    /// detach/re-attach cycle on the same OS thread is correct too).
    static FOREIGN_CRASH_FRAMES: std::cell::RefCell<
        Option<crate::vm::PublishedFrameTrace>,
    > = std::cell::RefCell::new(None);
    /// gc-common w10-c: the VM this OS thread's foreign attachment belongs to,
    /// recorded by [`attach_foreign_thread`] and read by every transition of
    /// the attachment ([`foreign_attachment_vm`]). The transitions used to
    /// resolve `process_vm()`, the most recently created VM, so with a second
    /// VM in the process an attached thread of the first left and re-entered
    /// the SECOND VM's barrier: it ran Java as a counted mutator of a VM whose
    /// pauses never counted it. `None` when the attaching `SharedVm` had no
    /// `self_arc` (a bare test fixture); a recorded VM that has since been
    /// dropped resolves to nothing, never to another VM.
    static FOREIGN_ATTACH_VM: std::cell::RefCell<Option<Weak<SharedVm>>> =
        const { std::cell::RefCell::new(None) };
    /// gc-common w10-c (`CRATONVM_JNI_FOREIGN_TRANSITIONS`): the depth of the
    /// attach-level local frame [`attach_foreign_thread`] opened, closed by
    /// [`detach_foreign_thread`]. `None` with the flag off.
    static FOREIGN_ATTACH_FRAME: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The VM this OS thread's foreign attachment belongs to (gc-common w10-c,
/// [`FOREIGN_ATTACH_VM`]). Falls back to the thread's JNI context and then to
/// `process_vm()` only when the attachment recorded no VM. `try_with`
/// throughout: the detach path can run during TLS teardown.
fn foreign_attachment_vm() -> Option<Arc<SharedVm>> {
    let recorded = FOREIGN_ATTACH_VM
        .try_with(|c| c.try_borrow().ok().and_then(|w| w.clone()))
        .ok()
        .flatten();
    if let Some(weak) = recorded {
        return weak.upgrade();
    }
    JNI_SHARED_VM
        .try_with(|c| c.try_borrow().ok().and_then(|b| b.clone()))
        .ok()
        .flatten()
        .or_else(process_vm)
}

/// The VM a C JVMTI function called on this OS thread acts on (interpreter
/// round i1 wave 12, `jvmti::native_env`): the thread's JNI context, else
/// `process_vm()`. `None` before any VM exists (an agent's `Agent_OnLoad`
/// runs while the first VM is being built).
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn calling_thread_vm() -> Option<Arc<SharedVm>> {
    JNI_SHARED_VM
        .try_with(|c| c.try_borrow().ok().and_then(|b| b.clone()))
        .ok()
        .flatten()
        .or_else(process_vm)
}

// ---------------------------------------------------------------------------
// An attachment whose OS thread exits without detaching (gen r5w1/crash5)
// ---------------------------------------------------------------------------
//
// The attachment's `JvmThread` lives in `FOREIGN_THREAD_BOX`, and its registry
// entry publishes two raw addresses into it: `tlab_addr` and `jvm_thread_addr`.
// `detach_foreign_thread_idle` retires the TLAB, clears both addresses and
// marks the entry dead before the box is dropped. A thread that EXITS attached
// never runs it: a native pool that attaches workers and lets them end, or a
// `DetachCurrentThread` that only arrives from a `pthread` TSD destructor (the
// note below: by then this key is destroyed, `is_foreign_attached()` answers
// `false`, and the detach does nothing). The key's own destructor then freed
// the box and left the entry ALIVE, idle-blocked and pointing into the freed
// allocation, for the rest of the process:
//
// * every pause's `ThreadRegistry::collect_reserved_tlab_tails` dereferences
//   `tlab_addr` -- a read of freed allocator memory -- and whatever
//   `(cursor, end)` it finds there is published as a TLAB skip span, which
//   every heap walk of all three collectors treats as "not objects": a span
//   that happens to cover live objects hides them from the mark and the sweep;
// * the entry never leaves the census, and its `os_tid` is signalled by every
//   helper-window pass until the kernel reuses the tid.
//
// `ForeignThreadBox` gives the box a destructor that does the detach's
// registry half first, TLS-free (`GcBarrier::leave_blocked_region_at_thread_teardown`),
// and LEAKS the `JvmThread` instead of freeing it whenever that cannot be done
// safely (the thread is not idle, or the attachment recorded no VM), so no
// published address can ever dangle. Correctness fix on a failure path
// (exiting attached), default-on; a detached attachment is unaffected.

/// The owner of a foreign attachment's `JvmThread`: the value of
/// [`FOREIGN_THREAD_BOX`]. Dereferences to the `JvmThread`.
pub(crate) struct ForeignThreadBox {
    /// The heap-boxed thread. `ManuallyDrop` because the reaping destructor
    /// may have to leak it (see the section comment above).
    jt: std::mem::ManuallyDrop<Box<JvmThread>>,
    /// `true` while the registry entry is live and nothing has detached it:
    /// dropping the box then has to reap the entry first.
    armed: bool,
    /// The VM the entry is registered in (`None`: the attaching `SharedVm`
    /// had no `self_arc`, so no reap is possible and an armed box is leaked).
    vm: Option<Weak<SharedVm>>,
    /// gcd d4/k2 (lane m's request 1): the pause ledger this attachment is
    /// counted in as holding RAW JNI locals
    /// (`gc_quiescence::note_raw_jni_locals_open`), when it is; closed exactly
    /// once, by the detach or by this box's drop (an OS thread that exits
    /// attached holds nothing any more, whether it is reaped or leaked).
    raw_locals_ledger: Option<Arc<cratonvm_gc::gc_quiescence::PauseLedger>>,
}

impl ForeignThreadBox {
    /// A box for an attachment registered in `vm`'s registry.
    fn attached(jt: Box<JvmThread>, vm: Option<Weak<SharedVm>>) -> Self {
        ForeignThreadBox {
            jt: std::mem::ManuallyDrop::new(jt),
            armed: true,
            vm,
            raw_locals_ledger: None,
        }
    }

    /// A box with no registry entry behind it (unit tests that fake an
    /// attachment): dropping it just frees the `JvmThread`.
    #[cfg(test)]
    fn unregistered(jt: Box<JvmThread>) -> Self {
        ForeignThreadBox {
            jt: std::mem::ManuallyDrop::new(jt),
            armed: false,
            vm: None,
            raw_locals_ledger: None,
        }
    }

    /// Close this attachment's raw-JNI-locals count, if it holds one.
    fn close_raw_locals(&mut self) {
        if let Some(ledger) = self.raw_locals_ledger.take() {
            cratonvm_gc::gc_quiescence::note_raw_jni_locals_closed(&ledger);
        }
    }

    /// The `JvmThread`, for a detach that has done (or is about to do) the
    /// registry half itself: disarms the reaping destructor.
    fn into_detached(mut self) -> Box<JvmThread> {
        self.close_raw_locals();
        self.armed = false;
        self.vm = None;
        // SAFETY: `jt` is taken exactly once, here, and `self` is forgotten
        // immediately after, so its destructor never sees the moved-out box.
        // The other fields left to forget are `vm` and `raw_locals_ledger`,
        // both already `None`.
        let jt = unsafe { std::mem::ManuallyDrop::take(&mut self.jt) };
        std::mem::forget(self);
        jt
    }
}

impl std::ops::Deref for ForeignThreadBox {
    type Target = JvmThread;
    fn deref(&self) -> &JvmThread {
        &self.jt
    }
}

impl std::ops::DerefMut for ForeignThreadBox {
    fn deref_mut(&mut self) -> &mut JvmThread {
        &mut self.jt
    }
}

impl Drop for ForeignThreadBox {
    fn drop(&mut self) {
        let armed = self.armed;
        let free = if !armed {
            true
        } else {
            match self.vm.take() {
                // The VM this attachment registered in is gone, and its
                // registry with it: nothing can read the published addresses.
                Some(weak) => match weak.upgrade() {
                    Some(shared) => reap_undetached_foreign_thread(&shared, &mut self.jt),
                    None => true,
                },
                // Registered, but no VM to reach the registry through.
                None => false,
            }
        };
        if armed && cratonvm_gc::root_write_audit::enabled() {
            crate::threading::thread_registry::note_undetached_attachment(
                self.jt.thread_id.0,
                free,
            );
        }
        if free {
            // SAFETY: dropped exactly once, here; `self` is being dropped and
            // `jt` is never touched again.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.jt) };
        } else {
            crate::threading::thread_registry::FOREIGN_ATTACHMENTS_LEAKED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // gcd d4/k2: an OS thread that exits attached holds no local any more,
        // reaped or leaked; its raw-locals count closes either way.
        self.close_raw_locals();
    }
}

/// The registry half of `DetachCurrentThread` for an attachment whose OS thread
/// is exiting attached, run from its thread-local destructor. Returns whether
/// the `JvmThread` may now be freed (`false`: leak it).
///
/// Only an IDLE attachment is reaped: it is counted in the barrier's blocked
/// population and excluded from every pause, so it may wait a pause out
/// (`leave_blocked_region_at_thread_teardown`). A thread exiting in the middle
/// of a Java call is a counted mutator that no pause can finish without; it is
/// left exactly as it was, and its memory is kept valid by the leak.
///
/// Thread-local-free throughout: this thread's other keys (the JNI context,
/// `FOREIGN_CALL_DEPTH`, the thread-state shadow cell) may already be
/// destroyed. What the ordinary detach does through them is either already
/// done at idle (the TLAB was retired, the snapshot deposited by the last
/// `foreign_enter_idle`) or dies with the thread (the JNI local frames, the
/// thread-local `ThreadLocal` map, whose values stay JNI global roots -- the
/// same leak an unreleased detach always had).
fn reap_undetached_foreign_thread(shared: &SharedVm, jt: &mut JvmThread) -> bool {
    use std::sync::atomic::Ordering;
    if !jt.gc_block_state.in_blocked_region.load(Ordering::Acquire) {
        return false;
    }
    let tid = jt.thread_id;
    // Drains through `try_with` (see `satb::flush_thread_satb_buffer`); an
    // already-destroyed buffer parked its residue as an orphan.
    shared.mem.heap.flush_thread_satb();
    shared
        .mem
        .gc_barrier
        .leave_blocked_region_at_thread_teardown(|| {
            // Idle means retired (`foreign_enter_idle`); retire again only if
            // something handed the idle thread a buffer since.
            if jt.tlab.reserved_tail().is_some() {
                jt.tlab.retire();
            }
            shared.threads.thread_registry.clear_tlab_addr(tid);
            shared.threads.thread_registry.mark_dead(tid);
            shared.threads.monitors.release_monitors_held_by(tid);
        });
    crate::threading::thread_registry::FOREIGN_ATTACHMENTS_REAPED.fetch_add(1, Ordering::Relaxed);
    true
}

// ---------------------------------------------------------------------------
// Detaching from a `pthread` thread-specific-data destructor
// ---------------------------------------------------------------------------
//
// `DetachCurrentThread` does not only arrive from application code. The
// libraries that attach host threads register their own per-thread cleanup
// with `pthread_key_create` (BoringSSL and APR through netty_tcnative, and
// the native transports underneath Vert.x), and glibc runs those destructors
// in `__nptl_deallocate_tsd` — which is *after* `__call_tls_dtors`, i.e.
// after every Rust `thread_local!` on that thread has already been destroyed.
//
// MEASURED (glibc 2.39 / Linux 6.17, rustc 1.97.1): in that phase every
// `LocalKey::try_with` on the thread answers `Err(AccessError)` — whether or
// not the key was ever initialized — while during Rust's *own* TLS
// destructors even a never-touched key still initializes normally. So the
// TSD phase is all-or-nothing: a detach arriving there can reach none of the
// attachment state below.
//
// `LocalKey::with` PANICS in that state rather than returning an error, and
// `jni_detach_current_thread` is an `extern "C"` function reached from a C
// destructor, so the unwind is undefined behaviour, not a diagnosable
// failure. What it did in practice was worse than either: the panic hook ran,
// touched thread-locals of its own, and the resulting panic-while-panicking
// aborted the process with SIGABRT — on 183 of 206 Hibernate Reactive classes
// under `--jdk-only`, every one of them *after* the class had already printed
// its passing `@@RESULT`. See
// `hibernate-reactive-double-panic-abort-FIXED-20260901`.
//
// Every thread-local access on the detach path therefore goes through
// `try_with` with a defined answer for "the thread is already gone". A detach
// that arrives in the TSD phase has nothing left to do: `FOREIGN_THREAD_BOX`'s
// own destructor has already run -- and, since gen r5w1/crash5, has done the
// registry half of the detach first (`ForeignThreadBox`), so the entry is no
// longer left alive pointing into the freed `JvmThread`.

/// True if the calling OS thread is currently foreign-attached (owns a
/// `JvmThread` parked in [`FOREIGN_THREAD_BOX`]).
///
/// `false` once this thread's TLS has been destroyed — see the note above: at
/// that point the box is gone, so "not attached" is the truthful answer and
/// the only one that can be given without panicking.
pub fn is_foreign_attached() -> bool {
    FOREIGN_THREAD_BOX
        .try_with(|c| c.borrow().is_some())
        .unwrap_or(false)
}

/// Build, register, and install a foreign (host-created) OS thread as a
/// first-class, GC-safe Java thread, returning the stable `*mut JvmThread` the
/// caller installs into `JNI_THREAD`.
///
/// This mirrors the per-thread wiring the main thread gets in `Vm::new`
/// (`vm_init.rs`) and that `Thread.start` workers get (`vm_exec.rs`): a
/// heap-boxed `JvmThread` with a fresh `ThreadId`, registered in the
/// `ThreadRegistry`, with its shared `Arc` fields (interrupted / park_state /
/// root_snapshot / frame_trace / gc_block_state) mirrored into the registry so a
/// GC initiator on another thread can scan and maintain this thread's roots.
///
/// Ordering is deliberate (see §3.2 / §5 of the design): the thread is
/// registered and its `root_snapshot` / `gc_block_state` are shared with the
/// registry BEFORE the caller publishes `JNI_SHARED_VM` / `JNI_THREAD`. So the
/// first instant this thread can run Java (and trip a safepoint) it is already
/// stop-the-world-visible with a (currently empty) deposited snapshot — there is
/// no window where it holds live oops but is invisible to `request_stw`.
///
/// The entry is COUNTED (`stw_ready`) from registration on, so a caller that
/// then models the thread as idle must not leave a gap before it raises
/// `in_blocked_region`: [`attach_foreign_thread_idle`] uses the starting form
/// instead (gcd d2/i). Test-only since then: production attaches go
/// through [`attach_foreign_thread_idle`].
#[cfg(test)]
pub(crate) fn attach_foreign_thread(
    shared: &SharedVm,
    daemon: bool,
    name: Option<&str>,
) -> *mut JvmThread {
    attach_foreign_thread_registered(shared, daemon, name, true)
}

/// [`attach_foreign_thread`], registering the entry counted (`stw_ready`) or,
/// with `stw_ready == false`, as STARTING: alive but in no pause's `expected`
/// until the caller runs `ThreadRegistry::mark_stw_ready`.
fn attach_foreign_thread_registered(
    shared: &SharedVm,
    daemon: bool,
    name: Option<&str>,
    stw_ready: bool,
) -> *mut JvmThread {
    let tid = shared.threads.thread_registry.next_thread_id();
    // Caller-supplied name (from `JavaVMAttachArgs.name`) when present, else the
    // JDK's default platform-thread naming `Thread-N`. A real java.lang.Thread
    // object + group is deferred to §3.5.
    let name = match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => format!("Thread-{}", tid.0),
    };
    let mut jt = Box::new(JvmThread::new(tid, &name));
    jt.kind = crate::threading::ThreadKind::Platform;
    jt.daemon = daemon;

    // Register, then replace the registry entry's default Arcs with this
    // JvmThread's own so the two share state (identical to vm_exec.rs's worker
    // wiring). register_with_daemon constructs fresh Arcs; the set_* calls
    // overwrite them with the thread-owned ones.
    if stw_ready {
        shared
            .threads
            .thread_registry
            .register_with_daemon(tid, &name, None, daemon);
    } else {
        shared
            .threads
            .thread_registry
            .register_starting_with_daemon(tid, &name, None, daemon);
    }
    // obsaudit D1: `attach_foreign_thread` runs synchronously on the
    // attaching OS thread, so bind the JVMTI thread-attribution TLS here —
    // classes this thread loads after attaching now report the real
    // `jthread` instead of the "unknown" sentinel.
    cratonvm_classloading::set_current_thread_id(shared.classes.class_layout_domain, tid.0);
    shared
        .threads
        .thread_registry
        .set_interrupted_flag(tid, jt.interrupted.clone());
    shared
        .threads
        .thread_registry
        .set_park_state(tid, jt.park_state.clone());
    shared
        .threads
        .thread_registry
        .set_root_snapshot(tid, jt.root_snapshot.clone());
    shared
        .threads
        .thread_registry
        .set_frame_trace(tid, jt.frame_trace.clone());
    // CR-VXC-3: the registry copy above serves cross-thread readers (thread
    // dumps); this one serves the crash handler, which runs *on* the faulting
    // thread and therefore reads a lock-free thread-local instead. Parked in
    // TLS so it outlives this call — see `FOREIGN_CRASH_FRAMES`. Cleared in
    // `detach_foreign_thread`.
    {
        // Drop any stale guard left by an earlier attachment on this OS thread
        // BEFORE publishing the new one. The guard is save-and-restore, so
        // dropping it *after* the new publication would restore the pre-attach
        // cell over the trace we just installed.
        let stale =
            FOREIGN_CRASH_FRAMES.with(|c| c.try_borrow_mut().ok().and_then(|mut slot| slot.take()));
        drop(stale);
        let guard = crate::vm::PublishedFrameTrace::publish(&name, tid.0, jt.frame_trace.clone());
        FOREIGN_CRASH_FRAMES.with(|c| {
            if let Ok(mut slot) = c.try_borrow_mut() {
                *slot = Some(guard);
            }
        });
    }
    shared
        .threads
        .thread_registry
        .set_gc_block_state(tid, jt.gc_block_state.clone());
    // BUG-03 — publish this foreign thread's TLAB address (the box is
    // address-stable) so the cross-thread STW JIT root scan can recover its
    // un-retired reserved tail if forcibly stopped mid-JIT. Cleared in
    // `detach_foreign_thread` before the box is dropped.
    shared
        .threads
        .thread_registry
        .set_tlab_addr(tid, &jt.tlab as *const cratonvm_gc::Tlab as usize);
    // XT-FRAME-SCAN: publish this foreign thread's `JvmThread` address too
    // (the same Box keeps it stable) so a takeover that freezes it mid-JIT
    // can walk its interpreter frames. Cleared with the TLAB address in
    // `detach_foreign_thread`.
    shared
        .threads
        .thread_registry
        .set_jvm_thread_addr(tid, &*jt as *const JvmThread as usize);

    // Park the box in TLS so it outlives this call and stays address-stable; the
    // raw pointer is the heap allocation address, unchanged by moving the Box.
    let raw: *mut JvmThread = &mut *jt as *mut JvmThread;
    let attach_vm = shared.try_get_arc().map(|a| Arc::downgrade(&a));
    // gen r5w1/crash5: the box reaps this registration if the OS thread exits
    // without detaching (see `ForeignThreadBox`).
    FOREIGN_THREAD_BOX
        .with(|c| *c.borrow_mut() = Some(ForeignThreadBox::attached(jt, attach_vm.clone())));
    FOREIGN_CALL_DEPTH.with(|c| c.set(0));
    // gc-common w10-c: remember WHICH VM this attachment belongs to, so its
    // transitions never resolve another one (`foreign_attachment_vm`).
    FOREIGN_ATTACH_VM.with(|c| *c.borrow_mut() = attach_vm);
    // gc-common w10-c (`CRATONVM_JNI_FOREIGN_TRANSITIONS`): the attach-level
    // local frame. A local ref a JNI function hands this thread OUTSIDE a Java
    // call lives here until `DeleteLocalRef` or detach, as in HotSpot's
    // top-level handle block; each such function's exit deposits it
    // (`ForeignJniEntry`). Not a frame native code pushed, so an unbalanced
    // `PopLocalFrame` cannot pop it.
    if foreign_transitions_active() {
        let base = local_frame_depth();
        push_local_frame(16);
        FOREIGN_ATTACH_FRAME.with(|c| c.set(Some(base)));
    }
    raw
}

/// Tear down the foreign thread previously installed by
/// [`attach_foreign_thread`] on this OS thread: deregister it from the VM and
/// reclaim its `JvmThread`, retiring the TLAB. Returns `true` if a foreign
/// thread was reclaimed, `false` if this OS thread held no foreign attachment.
///
/// The caller is responsible for leaving any idle/blocked region first (so the
/// reclamation does not race a live stop-the-world — see §3.4) and for clearing
/// the `JNI_THREAD` / `JNI_SHARED_VM` TLS afterwards.
pub fn detach_foreign_thread(shared: &SharedVm) -> bool {
    let jt = FOREIGN_THREAD_BOX
        .try_with(|c| c.borrow_mut().take())
        .ok()
        .flatten();
    let mut jt = match jt {
        // Disarms the reaping destructor: the caller's barrier-serialized
        // leave (`detach_foreign_thread_idle`) is the registry half.
        Some(j) => j.into_detached(),
        None => return false,
    };
    let tid = jt.thread_id;
    // An exception this attachment reported to a debugger and never caught
    // keeps a weak handle; it dies with the attachment (interpreter wave 12).
    crate::runtime::interpreter::release_debugger_thread_state(shared, &mut jt);
    // SATB (G1MARK-2): drain this thread's per-thread SATB buffer before the
    // thread detaches — same rationale as the platform-thread exit path in
    // vm_exec.rs: unflushed entries in the thread-local buffer are dropped
    // with the TLS, losing the marker's only record of overwritten
    // references. Cheap no-op when no marking cycle is active.
    shared.mem.heap.flush_thread_satb();
    // Retire the TLAB: install its tail filler and reset, so the unfilled tail
    // is walkable BEFORE this thread stops publishing its tail.  Clearing the
    // registry entry or marking it dead first lets a later non-moving sweep
    // observe raw zeroed tail bytes with no owner from which to recover a skip
    // span, desynchronizing the linear walk.
    jt.tlab.retire();
    // The tail is now a real walker-visible filler, so it is safe to remove
    // the address before the boxed `JvmThread` is dropped.
    shared.threads.thread_registry.clear_tlab_addr(tid);
    // Drop out of `alive_count` / STW `expected` before reclaiming the TLAB so a
    // subsequent `request_stw` no longer waits for this thread.
    shared.threads.thread_registry.mark_dead(tid);
    // A thread torn down while blocked inside a native call made from within
    // a `synchronized` region never executes its `monitorexit` bytecode —
    // release anything it still holds so no future locker waits forever
    // (see `MonitorTable::release_monitors_held_by`).
    shared.threads.monitors.release_monitors_held_by(tid);
    // CR-VXC-3: un-publish the crash handler's view of this thread BEFORE the
    // `JvmThread` box is dropped. The guard only holds an `Arc` to the frame
    // trace, so no dangling reference is possible either way, but leaving it in
    // place would make a later fault on this (now plain host) OS thread render
    // the stack of a Java thread that no longer exists.
    let crash_guard = FOREIGN_CRASH_FRAMES
        .try_with(|c| c.try_borrow_mut().ok().and_then(|mut slot| slot.take()))
        .ok()
        .flatten();
    drop(crash_guard);
    drop(jt);
    let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
    // gc-common w10-c: close the attach-level local frame (and anything a
    // native pushed on top of it and never popped), and forget the VM.
    if let Some(base) = FOREIGN_ATTACH_FRAME.try_with(Cell::take).ok().flatten() {
        truncate_local_frames(base);
    }
    let _ = FOREIGN_ATTACH_VM.try_with(|c| c.try_borrow_mut().map(|mut w| w.take()));
    true
}

/// Run `f` against this OS thread's foreign-attached `JvmThread`, if any.
///
/// Safe to call only at points where the interpreter does NOT also hold the
/// `&mut JvmThread` (which it borrows from the raw `JNI_THREAD` pointer): namely
/// the attach/detach paths and the foreign-call transitions, which run strictly
/// before a call begins or after it returns — never concurrently with the call.
fn with_foreign_thread<R>(f: impl FnOnce(&mut JvmThread) -> R) -> Option<R> {
    FOREIGN_THREAD_BOX
        .try_with(|c| c.borrow_mut().as_mut().map(|b| f(&mut **b)))
        .ok()
        .flatten()
}

/// Release the `ThreadLocal` values this foreign attachment set -- and the JNI
/// global root each object value owns -- before the attachment is torn down.
///
/// gc-common w4-g (`handoff-w3b-jni-detach-releases-thread-locals`), the JNI
/// half of `common-w2b-thread-local-values-of-dead-threads-are-global-roots`
/// (w3-b fixed the platform-thread death path). The values live in the
/// intrinsics' Rust `thread_local!` `TL_MAP` on the OS thread, which a
/// detach does not end: a host thread that attached, ran Java that set a
/// `ThreadLocal`, and detached kept every value a GC root for the life of the
/// VM, and -- the OS thread living on, as a native worker pool's does -- even
/// carried the values into its NEXT attachment, a different `java.lang.Thread`.
///
/// Runs as a counted mutator, exactly like a Java call on this thread
/// (`ForeignCallGuard`: wait out any pause, apply the fixups the idle window
/// accumulated, then re-deposit and go idle again). Not in the blocked region:
/// the inherited-bucket half of the release reads the `Thread` object's header
/// (`identity_hash_code`), and an idle thread's `java_thread_obj` is only
/// brought up to date by that wake. Not inside the
/// `mark_blocked_region_leave_after` closure either: it takes the JNI global
/// table lock, and that closure holds the barrier's transition lock.
///
/// Degrades to a no-op (the old behaviour) when this thread's TLS is being torn
/// down -- the transition uses `LocalKey::with` on these keys, which panics out
/// of an `extern "C"` frame there -- when a call is in flight, or when the JNI
/// binding does not name this attachment.
fn release_foreign_thread_locals() {
    let tls_intact = JNI_LOCAL_FRAMES.try_with(|_| ()).is_ok()
        && JNI_SHARED_VM.try_with(|_| ()).is_ok()
        && JNI_THREAD.try_with(|_| ()).is_ok()
        && JNI_CONTEXT_GENERATION.try_with(|_| ()).is_ok();
    if !tls_intact || FOREIGN_CALL_DEPTH.try_with(|c| c.get()).unwrap_or(1) != 0 {
        return;
    }
    let Some(me) = with_foreign_thread(|jt| jt.thread_id) else {
        return;
    };
    // `with_jni_context` reaches the `JvmThread` through the raw binding (as
    // every up-call does) rather than through `FOREIGN_THREAD_BOX`'s `RefCell`,
    // which must not stay borrowed across code that can allocate.
    if jni_bound_thread_id() != Some(me) {
        return;
    }
    let _fg = ForeignCallGuard::enter();
    let _ = with_jni_context(|shared, thread| {
        cratonvm_native_builtins::phases_early::release_current_thread_locals(
            &mut crate::vm::NativeContextImpl { shared, thread },
        )
    });
}

/// Declare the **current OS thread** as parked in host-native code so a garbage
/// collection driven by another (e.g. attached) thread does not wait for it to
/// reach a Java safepoint it will never hit while parked outside the VM.
///
/// This is the host-facing analog of HotSpot's `_thread_in_native`: it excludes
/// the thread from the stop-the-world `expected` set for the duration. Without
/// it, an idle creating/coordinator thread that sits in a host `join()` / event
/// loop while foreign threads drive GC is still counted as a live mutator and
/// **hangs the collection** (the deadlock the foreign-attach soak surfaced).
///
/// Must be balanced by exactly one [`host_thread_leave_native`]. Returns `false`
/// (a no-op) if no VM exists.
///
/// Intended for a thread that holds **no live Java roots** while parked — an idle
/// coordinator/creating thread. A **foreign attached** thread is already modelled
/// as in-native (GC-blocked) between its JNI calls, so this is a no-op for it
/// (double-counting would corrupt the barrier accounting).
///
/// CENSUS-RECONCILE (2026-07-31): this used to bump ONLY the barrier's anonymous
/// `threads_blocked` counter. Production STW does not compute `expected` from
/// that counter — it computes it from the IDENTITY census
/// (`request_stw_counted_with_live_blocked` ->
/// `ThreadRegistry::alive_count_blocked_and_os_tids`, `runtime/interpreter.rs`),
/// which excludes exactly the alive+`stw_ready` threads publishing
/// `in_blocked_region == true`. A registered thread that declared itself
/// host-native was therefore STILL counted in `expected` while parked in a host
/// `join()` it will not return from — the very hang this function exists to
/// prevent, and the exact shape the always-on `[gcbarrier-tripwire]` in
/// `stw_take_over_and_wait` reports ("a thread called
/// `mark_blocked_region_enter()` WITHOUT first depositing a root snapshot ...
/// invisible to the production STW census but still occupies an `expected` slot
/// no arrival can ever satisfy"). Publishing the identity half first — via the
/// same `mark_native_thread_blocked` + `mark_blocked_region_enter` +
/// `arrive_and_wait_auto` sequence `VmNativeThreadBlocker` (`vm/src/vm/vm_exec.rs`)
/// already uses for VM-registered native carrier threads — closes it.
pub fn host_thread_enter_native() -> bool {
    if is_foreign_attached() || in_native_jni_call() {
        // Already auto-managed as idle-blocked between calls; nothing to do.
        // gcd d3/k: likewise a thread inside a JNI native that is already in
        // native (`CRATONVM_JNI_NATIVE_TRANSITIONS`): the marking below would
        // empty the root snapshot its native call deposited (its Java frames
        // and JNI locals) and count it blocked twice.
        return true;
    }
    // gcd d3/k: the calling thread's own VM (its JNI context) before the
    // newest one, as `foreign_attachment_vm` does for an attachment. With one
    // VM in the process they are the same.
    let shared = match calling_thread_vm() {
        Some(s) => s,
        None => return false,
    };
    // gcd d10/j (`docs/internal/gc/gcd-d10j-host-native-inside-a-jni-native-empties-live-roots-FIXED-20260929.md`):
    // called from inside a JNI native of a VM thread that is NOT in native
    // (the flags off, or a raw local kept the call counted), the thread has
    // live Java frames and JNI locals under the native. The marking below
    // EMPTIES its root snapshot and excludes it from every pause -- its
    // frames' objects would be collected or moved under it. Go into the
    // blocked region the way every blocking native does instead: deposit the
    // real snapshot, then count the thread blocked.
    if let Some(thread) = jni_native_dispatch_thread() {
        // SAFETY: `JNI_THREAD` is this OS thread's dispatching `JvmThread`,
        // installed by the native's `JniContextGuard` for the whole call; the
        // interpreter holding it is suspended in the native, so this `&mut`
        // is the only one while it lives (as in `native_call_enter_native`).
        let jt = unsafe { &mut *thread };
        jt.tlab.retire();
        let tid = jt.thread_id;
        crate::vm::NativeContextImpl {
            shared: &*shared,
            thread: jt,
        }
        .deposit_root_snapshot();
        if shared.mem.gc_barrier.mark_blocked_region_enter() {
            // The flag went up in the deposit, before this check: `auto`
            // arrives only if the pause in progress counted the thread.
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
        return true;
    }
    // Identity of the calling host thread, if the VM registered it (the
    // creating thread is `ThreadId(0)`, published by `Vm::new`). `None` means
    // this OS thread is in no registry entry at all, so it is absent from
    // `alive_count` and occupies no `expected` slot — the counter-only path
    // below is then exactly right.
    //
    // gc-common w2-c: the JNI binding is consulted first (exact), then the OS
    // tid (for a carrier, the mounted virtual thread since w6-g), and
    // `host_thread_leave_native` resolves the same way, so the flag raised
    // here is the flag lowered there.
    let tid = jni_caller_thread_id(&shared);
    if let Some(tid) = tid {
        // Raise the identity flag BEFORE the counter, matching every other
        // blocking-region entry (`deposit_root_snapshot` then
        // `mark_blocked_region_enter`). A census serialized before this sees a
        // running mutator and counts us — `pre_stw` + `arrive_and_wait_auto`
        // below then supply the arrival it is waiting for; one serialized
        // after sees the flag and excludes us.
        //
        // `mark_native_thread_blocked` also empties the published root
        // snapshot, which is the contract stated above: a thread parked in
        // HOST code holds no live Java roots. (Its `java.lang.Thread` mirror
        // is not affected — that is a strong root of every alive registry
        // entry, `memory/roots.rs` step 10b.)
        shared
            .threads
            .thread_registry
            .mark_native_thread_blocked(tid);
    }
    let pre_stw = shared.mem.gc_barrier.mark_blocked_region_enter();
    if pre_stw {
        // A stop-the-world was already active when we incremented the blocked
        // count. Whether it counted us in `expected` depends on which side of
        // the flag raise above its census landed, which is NOT decidable here
        // — so resolve it from that pause's own exclusion snapshot
        // (GCAUDIT-0711-FIX finding 1a): `auto` arrives exactly once if the
        // census counted us, and only waits the pause out if it excluded us.
        //
        // `None` does NOT arrive (gc-common w2-c). It used to arrive as
        // `ThreadId(0)` -- main's real id -- which with main NOT the initiator
        // and counted fills main's quota slot: the pause is released while
        // main still runs. An unregistered caller is in no census and owes no
        // arrival; `mark_blocked_region_leave` in the matching leave waits the
        // pause out. (An AMBIGUOUS caller without a JNI binding is counted and
        // not arriving can hang this pause -- the conservative failure, and
        // unreachable in practice: see `jni_caller_thread_id`.)
        if let Some(tid) = tid {
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
    }
    true
}

/// Re-enter the VM after [`host_thread_enter_native`]: the current thread rejoins
/// the mutator population (it will wait out any in-flight stop-the-world first).
/// Must balance exactly one prior `host_thread_enter_native`. No-op for a foreign
/// attached thread (auto-managed) or when no VM exists.
///
/// CENSUS-RECONCILE (2026-07-31): symmetric half of the fix described on
/// [`host_thread_enter_native`]. This used to drop only the anonymous counter,
/// so a thread that HAD been excluded by the identity census resumed with its
/// `in_blocked_region` flag still raised — permanently invisible to every later
/// pause (a running mutator that no collection waits for) and never draining the
/// blocked-window fixup those pauses accumulated for it. The sequence below is
/// the canonical one every blocking native uses
/// (`NativeContextImpl::end_blocking_region`, `vm/src/vm/vm_exec.rs`): drop the
/// counter (waiting out the pause we were excluded from), then clear the
/// identity flag through `leave_blocked_region_flagged`, which performs the
/// clear under the same barrier-lock hold that proved no pause is active — the
/// window a bare `store(false)` leaves open is finding 1(c)'s corruption family.
pub fn host_thread_leave_native() -> bool {
    if is_foreign_attached() || in_native_jni_call() {
        return true;
    }
    // Resolved exactly as `host_thread_enter_native` resolved it.
    let shared = match calling_thread_vm() {
        Some(s) => s,
        None => return false,
    };
    // gcd d10/j: the matching leave of the deposit path above -- the same
    // answer, since the native that made both calls is still on the stack.
    // The canonical wake: wait out a pause, clear the flag, apply what every
    // collection of the window folded into the thread (frames, JNI locals,
    // compiled frames) and re-deposit.
    if let Some(thread) = jni_native_dispatch_thread() {
        shared.mem.gc_barrier.mark_blocked_region_leave();
        // SAFETY: as in `host_thread_enter_native`.
        crate::vm::NativeContextImpl {
            shared: &*shared,
            thread: unsafe { &mut *thread },
        }
        .check_post_block_gc();
        return true;
    }
    // Resolved exactly as `host_thread_enter_native` resolved it.
    let tid = jni_caller_thread_id(&shared);
    shared.mem.gc_barrier.mark_blocked_region_leave();
    if let Some(tid) = tid {
        if let Some(gc_block_state) = shared.threads.thread_registry.gc_block_state_of(tid) {
            shared
                .mem
                .gc_barrier
                .leave_blocked_region_flagged(tid, &gc_block_state.in_blocked_region);
        }
        // Drain the blocked-window side tables now that the flag is down and
        // no further fold can target us. This is the host-thread analogue of
        // `check_post_block_gc`'s fixup application: there is nothing to apply
        // it TO (the deposited snapshot was emptied at enter, and a host thread
        // owns no interpreter frames while parked outside the VM), so the
        // accumulated map is discarded — `mark_native_thread_unblocked` reports
        // a non-empty one under `CRATONVM_DBG_BLOCKGC`, which is the signal
        // that a caller broke the "no live Java roots while parked" contract.
        // Its own `store(false)` is a no-op here: the barrier already cleared
        // the flag above.
        shared
            .threads
            .thread_registry
            .mark_native_thread_unblocked(tid);
    }
    true
}

// ---------------------------------------------------------------------------
// Asynchronous-I/O completion dispatcher
// ---------------------------------------------------------------------------

// gc-common w11-b (`common-w10c-aio-dispatcher-is-one-per-process`): there is
// one dispatcher pool PER VM. It used to be one per process
// (`AIO_DISPATCH_STARTED`), attached to `process_vm()` -- the newest VM -- and
// draining process-wide queues, so one VM's completions were resolved in
// another VM's global-ref table and run as that VM's mutator. The per-VM
// "started" and "shut down" states now live with the VM's queues in
// `native-io` (`async_socket::AioVmState`): `ensure_dispatcher` launches once
// per VM, and `forget_vm_aio_state` (VM teardown) ends that VM's loops.

/// Number of AIO completion-dispatcher threads.
///
/// MUST be >1. A single dispatcher thread can self-deadlock: delivering one
/// completion means synchronously invoking Java (a `CompletionHandler`, or
/// completing a real JDK `CompletableFuture` which unparks its waiters), and
/// that Java code is allowed — by the real `AsynchronousSocketChannel`
/// contract this VM is emulating — to itself perform a *different* blocking
/// async I/O call from within that callback (e.g. `Future.get()` on a write
/// issued from inside a handler-form read's `completed()`). On real JDK this
/// is safe because `AsynchronousChannelGroup`'s default group is a THREAD
/// POOL (`Runtime.availableProcessors()` threads), so a different pool thread
/// services the nested completion. With exactly one dispatcher thread, that
/// nested completion can only ever be delivered by the very thread that is
/// now blocked waiting for it — a hard deadlock (bounded only by whatever
/// timeout, if any, the blocked Java call happens to pass).
///
/// Found via: Tomcat's own client-side WebSocket implementation
/// (`org.apache.tomcat.websocket.WsFrameClient`) does exactly this on a
/// received close frame — its handler-form read completion callback
/// synchronously drives `WsSession.onClose()` -> `sendCloseMessage()` ->
/// `WsRemoteEndpointImplClient.doWrite()`, which blocks on a *Future-form*
/// `AsynchronousSocketChannel.write(...).get(timeout)` — all still on the
/// single `cratonvm-aio-dispatch` thread. See
/// CRATONVM-SPRING-GENUINE-BUGLIST section 5.8 for the
/// full trace that root-caused this (Spring's `WebSocketIntegrationTests`,
/// `TomcatWebSocketClient` parameterization).
///
/// A *fixed* pool still cannot make this class of reentrancy deadlock
/// provably impossible in the general case (nor does real JDK's own default
/// `AsynchronousChannelGroup`, which is also a fixed CPU-count-sized pool —
/// see its `CompletionHandler` Javadoc caveat about handlers that themselves
/// wait on another I/O operation in the same group). A fixed size of 4 was
/// tried first and still exhausted occasionally under this same test's
/// transient concurrency (multiple overlapping close-frame handshakes each
/// wanting a free dispatcher thread); a floor of 16 measured far fewer
/// residual occurrences across repeated runs (going as high as 32 did not
/// reliably improve on 16 further, suggesting the small remaining flake rate
/// is dominated by host scheduling noise rather than pool size — see
/// CRATONVM-SPRING-GENUINE-BUGLIST section 5.8). Sized
/// off CPU count with that floor, matching both real JDK's own approach and
/// the sizing convention already used for `native-io`'s AIO worker pool
/// (`async_socket.rs::start_pool`).
fn aio_dispatch_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(16)
}

/// Start the AIO completion dispatcher thread pool (idempotent).
///
/// `AsynchronousSocketChannel` handler-form reads run their blocking I/O on a
/// non-VM worker pool that parks completions but cannot invoke Java. Each
/// dispatcher is a *foreign-attached* VM thread: it sits idle in the GC-blocked
/// region waiting on the completion condvar, and whenever a read completes it
/// transitions to a running mutator (via [`ForeignCallGuard`]) just long enough
/// to invoke the Java `CompletionHandler`, then returns to idle. This mirrors
/// the dispatcher thread POOL of a real `AsynchronousChannelGroup` (plural —
/// see `aio_dispatch_thread_count`). Wired up by `native-io`'s
/// `set_dispatcher_launcher`, fired on a VM's first handler-form or
/// Future-form operation. Each VM's completion queues (`native-io`'s
/// `AioVmState`) are `Mutex`-protected `VecDeque`s popped one entry at a time, so multiple
/// dispatcher threads draining concurrently is race-free by construction —
/// no two threads can pop the same completion.
///
/// **Per VM** (gc-common w11-b). The launcher is one process-wide closure, so
/// the VM to serve is read from `native-io`'s `launching_vm()`, which its
/// `ensure_dispatcher` sets for the duration of the launch (it runs on a
/// thread of that VM, once per VM). A launch with no launching VM, or whose
/// VM is not live, starts nothing: it never falls back to `process_vm()`.
pub fn start_aio_dispatcher() {
    let Some(vm) = cratonvm_native_io::async_socket::launching_vm() else {
        return;
    };
    let Some(shared) = live_vm_by_identity(vm) else {
        return;
    };
    start_aio_dispatcher_for(&shared);
}

/// Spawn `shared`'s dispatcher pool: every thread attaches to THAT VM. The
/// once-per-VM guard is `native-io`'s (`ensure_dispatcher`), so this is only
/// reached through [`start_aio_dispatcher`].
fn start_aio_dispatcher_for(shared: &Arc<SharedVm>) {
    for i in 0..aio_dispatch_thread_count() {
        let shared = Arc::clone(shared);
        let _ = std::thread::Builder::new()
            .name(format!("cratonvm-aio-dispatch-{i}"))
            .spawn(move || aio_dispatcher_main(shared));
    }
}

/// The live VM whose `vm_identity` is `vm`, from the registry every
/// `Vm::new` publishes into ([`set_process_vm`]). `None` for a VM that is gone
/// or was never published. The `Weak`s are copied out first, so no `Arc` is
/// upgraded -- or dropped -- under the registry lock.
fn live_vm_by_identity(vm: usize) -> Option<Arc<SharedVm>> {
    let weaks: Vec<Weak<SharedVm>> = VM_REGISTRY.lock().clone();
    weaks
        .iter()
        .filter_map(Weak::upgrade)
        .find(|s| s.vm_identity == vm)
}

fn aio_dispatcher_main(shared: Arc<SharedVm>) {
    let vm = shared.vm_identity;
    // Attach as a foreign daemon thread and settle into the idle GC-blocked
    // region (the genuine-first-attach path of `attach_current_thread_impl`,
    // shared since gc-common w10-c). The empty deposited snapshot is published
    // before the TLS context, so there is no window where this thread is a
    // running mutator invisible to `request_stw`.
    let self_name = std::thread::current()
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| "cratonvm-aio-dispatch".to_string());
    attach_foreign_thread_idle(shared, true, Some(&self_name));

    // Serve this VM until it is disposed: `forget_vm_aio_state` (called from
    // `release_vm_native_state`) ends the loop and wakes a parked wait.
    while cratonvm_native_io::async_socket::aio_dispatcher_should_run(vm) {
        // Idle, GC-blocked, until one of THIS VM's completions is ready
        // (bounded poll so a missed wake can never wedge delivery).
        let pending = cratonvm_native_io::async_socket::wait_for_pending(
            vm,
            std::time::Duration::from_millis(100),
        );
        if !pending {
            continue;
        }
        // ForeignCallGuard leaves the idle region (becomes a counted mutator),
        // waiting out any in-flight STW first, and re-enters it on drop.
        let _fg = ForeignCallGuard::enter();
        let _ = with_jni_context(|shared, thread| {
            let mut ctx = crate::vm::NativeContextImpl { shared, thread };
            cratonvm_native_io::async_socket::drain_completions_pub(&mut ctx);
        });
    }

    // Clean detach (reached when this VM is disposed). Completions run Java
    // on this thread, which may set `ThreadLocal`s: released first.
    if let Some(shared) = foreign_attachment_vm() {
        detach_foreign_thread_idle(&shared);
    }
    clear_jni_thread();
    clear_jni_context();
}

/// A unit of work for [`run_attached_service`].
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) type AttachedServiceJob = Box<dyn FnOnce(&SharedVm, &mut JvmThread) + Send>;

/// Serve `jobs` on the calling OS thread, attached to `shared` as a daemon
/// foreign thread named `name` — the AIO dispatcher's pattern above: idle in
/// the GC-blocked region between jobs, a counted mutator
/// (`ForeignCallGuard`) while one runs. Sends the attachment's thread id on
/// `started` first; detaches when `jobs` closes. Interpreter round i1 wave
/// 10, lane L1: the JDWP server's heap service (`debug::start_heap_service`),
/// which allocates for `CreateString` / `ArrayType.NewInstance` when no
/// thread is parked at a suspend point.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn run_attached_service(
    shared: Arc<SharedVm>,
    name: &str,
    started: std::sync::mpsc::SyncSender<u64>,
    jobs: std::sync::mpsc::Receiver<AttachedServiceJob>,
) {
    attach_foreign_thread_idle(shared, true, Some(name));
    let tid = with_foreign_thread(|jt| jt.thread_id.0).unwrap_or(0);
    let _ = started.send(tid);
    while let Ok(job) = jobs.recv() {
        let _fg = ForeignCallGuard::enter();
        let _ = with_jni_context(|shared, thread| job(shared, thread));
    }
    if let Some(shared) = foreign_attachment_vm() {
        detach_foreign_thread_idle(&shared);
    }
    clear_jni_thread();
    clear_jni_context();
}

/// Round 13 wave 1 (lane mhffm patch): `panama_upcall::UpcallAttachHook`. An
/// FFM upcall on a thread with no active downcall runs here: on the thread's
/// own JNI binding when it has one for that VM (a JNI native calling a kept
/// stub, an attached host thread), else by attaching the OS thread as a
/// DAEMON the way HotSpot's `UpcallLinker::on_entry` does
/// (`AttachCurrentThreadAsDaemon`). The attachment is kept until the OS
/// thread exits (`ForeignThreadBox` reaps it), so a C worker that calls back
/// many times attaches once.
pub(crate) fn ffm_upcall_attach(
    vm: usize,
    run: &mut dyn FnMut(&mut dyn cratonvm_native_api::NativeContext),
) -> bool {
    if jni_bound_thread_id().is_none() {
        let Some(shared) = vm_for_identity(vm) else {
            return false;
        };
        attach_foreign_thread_idle(shared, true, None);
    }
    // Before `with_jni_context`: an idle foreign attachment leaves its
    // GC-blocked region here (`ForeignCallGuard`), and the guard needs its
    // own `&mut JvmThread` access first.
    let _call = ForeignCallGuard::enter();
    with_jni_context(|shared, jt| {
        if shared.vm_identity != vm {
            return false;
        }
        let mut ctx = crate::vm::NativeContextImpl { shared, thread: jt };
        run(&mut ctx);
        true
    })
    .unwrap_or(false)
}

/// The live VM whose `vm_identity` is `vm` (never the newest-VM guess of
/// `process_vm`).
fn vm_for_identity(vm: usize) -> Option<Arc<SharedVm>> {
    let candidates: Vec<Weak<SharedVm>> = {
        let mut reg = VM_REGISTRY.lock();
        reg.retain(|w| w.strong_count() > 0);
        reg.clone()
    };
    candidates
        .iter()
        .filter_map(Weak::upgrade)
        .find(|shared| shared.vm_identity == vm)
}

/// Attach the calling OS thread to `shared` as a foreign thread and settle it
/// into the idle GC-blocked region, then publish its JNI context: the
/// genuine-first-attach half of `AttachCurrentThread`, shared with the AIO
/// dispatcher (gc-common w10-c; it was written out twice). Returns the
/// attachment's `JvmThread`, as [`attach_foreign_thread`] does.
///
/// §3.3: an attached-but-idle thread (parked in the host event loop with no
/// Java frames) is modelled as GC-blocked, so a stop-the-world on another
/// thread does not wait forever for it. The matching leave is the first
/// transition out of idle: a Java call's `ForeignCallGuard`, a JNI function's
/// `ForeignJniEntry` (`CRATONVM_JNI_FOREIGN_TRANSITIONS`), or the detach.
fn attach_foreign_thread_idle(
    shared: Arc<SharedVm>,
    daemon: bool,
    name: Option<&str>,
) -> *mut JvmThread {
    // Build + register the foreign thread. It is STW-visible (with an empty
    // deposited snapshot) BEFORE the TLS context is published below -- there
    // is no window where it can run Java while invisible to `request_stw`.
    //
    // gcd d2/i (2026-09-27): registered STARTING, and made counted only once
    // `in_blocked_region` is up. It used to be registered counted
    // (`register_with_daemon`) and flagged blocked only after the half-dozen
    // registry writes of `attach_foreign_thread`: a pause whose census fell in
    // that window counted this thread in `expected`, and the `pre_stw` below
    // was then ignored on the belief that "we registered after its request" --
    // so the thread went idle in host code without ever arriving, and the
    // pause waited for it forever. Now no census can count it: before
    // `mark_stw_ready` it is not `stw_ready`, after it the census sees the
    // flag (stored first, Release; the census loads `stw_ready` then the flag,
    // Acquire) and excludes it.
    let raw = attach_foreign_thread_registered(&shared, daemon, name, false);
    let tid = with_foreign_thread(|jt| {
        jt.gc_block_state
            .in_blocked_region
            .store(true, std::sync::atomic::Ordering::Release);
        jt.thread_id
    });
    if let Some(tid) = tid {
        shared.threads.thread_registry.mark_stw_ready(tid);
    }
    // Counted by no pause (see above), so we must NOT arrive even if one is in
    // progress -- hence the pre_stw return is intentionally ignored here.
    let _ = shared.mem.gc_barrier.mark_blocked_region_enter();
    // Publish the TLS context LAST.
    set_jni_context_arc(shared);
    set_jni_thread(raw);
    raw
}

/// Tear down this OS thread's idle foreign attachment to `shared`: release its
/// `ThreadLocal` values while it is still registered, retire its TLAB and mark
/// it dead inside one barrier-serialized leave, then reclaim it
/// ([`detach_foreign_thread`]). The caller clears the JNI TLS afterwards.
/// Shared by `DetachCurrentThread` and the AIO dispatcher's shutdown
/// (gc-common w10-c).
fn detach_foreign_thread_idle(shared: &SharedVm) {
    // Drop this attachment's `ThreadLocal` values (their JNI global roots)
    // while it is still registered: `current_thread_object` needs the entry.
    // See `release_foreign_thread_locals`.
    release_foreign_thread_locals();
    let tid = with_foreign_thread(|jt| jt.thread_id);
    // Retire the tail, remove its published address, and mark dead in the
    // same barrier-serialized transition. A new STW must see neither an
    // unwalkable tail nor a dead thread that is still counted as blocked.
    //
    // Not the monitor sweep (round 12 wave 2, lane lock): it walks every
    // inflated monitor, and this closure holds the GC barrier's transition
    // lock, which every blocked-region enter and leave in the VM queues on.
    // `detach_foreign_thread` below sweeps right after, outside it (it ran the
    // sweep a second time already), as the platform thread's termination does
    // since round 12 wave 1; the walk tolerates a concurrent index re-key
    // (`MonitorTable::release_monitors_held_by_except`, `remap_seq`).
    if let Some(tid) = tid {
        shared.mem.gc_barrier.mark_blocked_region_leave_after(|| {
            with_foreign_thread(|jt| jt.tlab.retire());
            shared.threads.thread_registry.clear_tlab_addr(tid);
            shared.threads.thread_registry.mark_dead(tid);
        });
    } else {
        shared.mem.gc_barrier.mark_blocked_region_leave();
    }
    // Reclaim the already-retired attachment.
    detach_foreign_thread(shared);
}

/// Whether the foreign-thread attach path is enabled.
///
/// Default **ON** (Step 7 of `foreign-thread-attach.md`): a genuinely foreign
/// thread that calls `AttachCurrentThread` is registered as a first-class
/// GC-safe Java thread — the real `libjvm`-substitute behaviour. The opt-out
/// `CRATONVM_FOREIGN_ATTACH=0` (or `false`) restores the historical env-only
/// stub (no thread registration), matching the project rule that the real path
/// is the default and the legacy/synthetic path is the safety net. Read on each
/// attach (cold path; attach is rare).
fn foreign_attach_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_FOREIGN_ATTACH").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    )
}

/// `#[repr(C)]` mirror of jni.h `JavaVMAttachArgs` — the optional third argument
/// to `AttachCurrentThread` / `AttachCurrentThreadAsDaemon`:
/// ```c
/// typedef struct JavaVMAttachArgs { jint version; char *name; jobject group; }
/// ```
/// `group` is a landing spot for §3.5 (Java Thread object + thread group);
/// today only `name` is consumed, to give an attached thread a meaningful
/// registry name in dumps.
#[repr(C)]
pub struct JavaVMAttachArgs {
    pub version: JInt,
    pub name: *const c_char,
    pub group: JObject,
}

/// Best-effort read of the attach `name` from an optional `JavaVMAttachArgs*`.
///
/// # Safety
/// `args`, when non-null, must point at a valid `JavaVMAttachArgs` whose `name`
/// (when non-null) is a NUL-terminated C string — the JNI Invocation-API
/// contract for the argument.
unsafe fn read_attach_name(args: *mut std::ffi::c_void) -> Option<String> {
    if args.is_null() {
        return None;
    }
    let a = &*(args as *const JavaVMAttachArgs);
    if a.name.is_null() {
        return None;
    }
    CStr::from_ptr(a.name).to_str().ok().map(|s| s.to_string())
}

/// RAII guard bracketing a JNI call (`Call*Method` / `NewObject`) on a
/// foreign-attached thread, implementing the idle↔running transition of §3.3.
///
/// A foreign thread is modelled as **GC-blocked while idle** (between calls,
/// parked in the host event loop): it holds no Java frames and will not reach an
/// interpreter safepoint, so a stop-the-world on another thread must not wait
/// for it. On the **outermost** Java call this guard leaves the blocked region —
/// becoming a counted mutator whose interpreter polls safepoints normally — and
/// on return re-enters it. Nested calls (a JNI up-call made by a native that the
/// interpreter dispatched mid-call) only adjust the depth counter; the thread is
/// already a counted mutator. For non-foreign threads (the bootstrap/creating
/// thread, VM `Thread.start` workers making up-calls) the guard is entirely
/// inert.
///
/// With `CRATONVM_JNI_FOREIGN_TRANSITIONS` on, the JNI function that makes
/// the call has already done the transition (`ForeignJniEntry`), so this guard
/// runs nested: no per-call frame, and what the call hands back lands in the
/// attach-level frame. It still transitions for its other users (the AIO
/// dispatcher, the detach-time `ThreadLocal` release).
struct ForeignCallGuard {
    /// `Some(vm)` iff this guard performed the outermost idle→running transition
    /// and must perform the running→idle transition on drop.
    transition: Option<Arc<SharedVm>>,
    /// `true` iff this guard incremented [`FOREIGN_CALL_DEPTH`] (i.e. the thread
    /// is foreign-attached) and must decrement it on drop.
    counted: bool,
    /// The local-frame depth before this guard opened its per-call frame
    /// (meaningful only with `transition`): the drop closes every frame from
    /// there up (gc-common w7-c, [`truncate_local_frames`]).
    frames_base: usize,
}

impl ForeignCallGuard {
    fn enter() -> Self {
        if !is_foreign_attached() {
            return ForeignCallGuard {
                transition: None,
                counted: false,
                frames_base: 0,
            };
        }
        let prev = FOREIGN_CALL_DEPTH.with(|c| {
            let p = c.get();
            c.set(p + 1);
            p
        });
        if prev != 0 {
            // Nested call (e.g. a JNI up-call) — already a counted mutator.
            return ForeignCallGuard {
                transition: None,
                counted: true,
                frames_base: 0,
            };
        }
        // Outermost call: leave the idle blocked region and become a counted
        // mutator. `mark_blocked_region_leave` waits out any in-flight STW first,
        // so we never start running Java under an active collection. The
        // attachment's own VM (gc-common w10-c): `process_vm()` answered the
        // newest VM, so a second VM in the process sent this leave to a
        // barrier the thread was never blocked in.
        let shared = match foreign_attachment_vm() {
            Some(s) => s,
            None => {
                return ForeignCallGuard {
                    transition: None,
                    counted: true,
                    frames_base: 0,
                }
            }
        };
        // BUG FIX (2026-07-18, aio-dispatch-sigsegv): this used to combine
        // the counter-based leave (`mark_blocked_region_leave_after`) with a
        // bare `in_blocked_region.store(false)` + `root_snapshot.lock().clear()`
        // done "atomically" under the barrier lock. That closed the identity-
        // census race its own comment describes, but it is NOT the same
        // operation as the canonical "done blocking, about to run Java again"
        // idiom every other blocking native in this codebase uses
        // (`NativeContextImpl::end_blocking_region`, `vm/src/vm/vm_exec.rs`):
        // `mark_blocked_region_leave()` (counter) followed by
        // `check_post_block_gc()` (identity-flag clear via
        // `GcBarrier::leave_blocked_region_flagged`, which independently
        // closes the same back-to-back-pause race by looping under its own
        // lock — no atomic combination with the counter needed). Critically,
        // `check_post_block_gc()` also APPLIES the accumulated cross-GC
        // `fixup` map to every persistent per-thread field — `java_thread_obj`,
        // `native_pin_roots`, `native_alloc_pool`, `native_pending_return`,
        // `scoped_values`, JNI locals — before
        // depositing a fresh snapshot. The old bare clear here skipped that
        // fixup application entirely: any GC that ran while this dispatcher
        // thread sat idle correctly relocated (not reclaimed — its snapshot
        // now correctly includes `java_thread_obj`, see `Drop` below) this
        // thread's own `java_thread_obj`, but nothing ever copied the new
        // post-move address back into `JvmThread.java_thread_obj` itself, so
        // the NEXT `Thread.currentThread()` call on this thread (directly, or
        // transitively — e.g. `ThreadLocal.get()`'s inherited-value drain)
        // handed back the stale pre-move address. That is the confirmed root
        // cause of the `cratonvm-aio-dispatch-N` "arbitrary call site" SIGSEGV
        // class (Result 3, CRATONVM-SPRING-GENUINE-BUGLIST
        // 5.8 follow-up #2) — reproduced live with
        // `CRATONVM_DBG_STALE_OBJREF=1`: `current_thread_object` ->
        // `identity_hash_code` -> `get_header` panics "stale ObjectRef ...
        // this object was evacuated by a moving GC to <addr> ... but
        // native/interpreter code dereferenced the OLD address" on exactly
        // this thread family, from `drain_inherited_for_current_thread`
        // (`native-builtins/src/phases_early.rs`). Using the two established
        // helper methods instead of reimplementing a partial version of them
        // closes the gap and keeps this transition in sync with any future
        // change to the canonical blocking-region discipline.
        foreign_leave_idle(&shared);
        // A fresh local-ref frame scopes this call's JNI local refs (freed on
        // return, per JNI semantics) so the thread holds none across the idle
        // window — keeping the idle root snapshot genuinely empty.
        let frames_base = local_frame_depth();
        push_local_frame(16);
        ForeignCallGuard {
            transition: Some(shared),
            counted: true,
            frames_base,
        }
    }
}

impl Drop for ForeignCallGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.transition.take() {
            // Outermost return → go idle. Close this call's local-ref frame and
            // any frame a native pushed inside it and leaked (gc-common w7-c:
            // popping ONE frame left this call's own frame open, a root held
            // across every idle window of the thread).
            truncate_local_frames(self.frames_base);
            // Retire the TLAB while still a counted mutator: a STW requested now
            // is still waiting for us to arrive (its collector has not started),
            // so writing the TLAB tail filler cannot race the moving collector.
            // Then deposit a REAL root snapshot (not a bare clear) and re-enter
            // the idle blocked region.
            //
            // BUG FIX (2026-07-18, aio-dispatch-sigsegv): this used to be
            // `jt.root_snapshot.lock().clear()` — correct for the *frame*
            // portion (frames are indeed empty once the outermost call has
            // returned) but wrong for everything else `deposit_root_snapshot`
            // captures: `java_thread_obj`, any
            // straggling JNI local refs, and JIT/shadow-stack roots. A
            // foreign-attached AIO dispatcher thread's `java_thread_obj` is
            // lazily allocated the first time Java code on this thread calls
            // `Thread.currentThread()` (directly, or transitively — e.g.
            // `ThreadLocal.get()`'s inherited-value drain calls
            // `ctx.current_thread_object()`) and then PERSISTS on `JvmThread`
            // across every subsequent completion this dispatcher thread ever
            // services. This idle transition runs after EVERY delivered
            // completion, so a bare `.clear()` made that persistent mirror
            // object GC-invisible for the entire idle window between
            // completions (the dispatcher's dominant time-in-state, since it
            // parks on a condvar in `wait_for_pending` between wakeups) —
            // exactly the "the deposited snapshot is the ONLY view a
            // cross-thread STW collector has of a parked thread's roots"
            // hazard `deposit_root_snapshot`'s own doc comment (and the
            // `interpreter::update_root_snapshot` mirror of it) describes.
            // Any GC that ran while this thread sat idle could relocate or
            // reclaim `java_thread_obj` without ever updating this thread's
            // copy, so the NEXT completion's `Thread.currentThread()` (or any
            // other read of the same persistent field) handed back a
            // stale/dangling `ObjectRef` — dereferenced at whatever call site
            // happened to touch it next (`identity_hash_code`, a `get_field`,
            // an array-bounds check, ...). That is the "essentially arbitrary
            // native/interpreter call site" `cratonvm-aio-dispatch-N` SIGSEGV
            // class documented as Result 3 in
            // CRATONVM-SPRING-GENUINE-BUGLIST's 5.8
            // follow-up #2 — confirmed live via `CRATONVM_DBG_STALE_OBJREF=1`,
            // which turns the segfault into a clean panic naming
            // `current_thread_object` -> `identity_hash_code` -> `get_header`
            // ("stale ObjectRef ... evacuated by a moving GC") on exactly this
            // thread family. `deposit_root_snapshot` is the established,
            // already-audited "about to block" discipline every other
            // idle/parked transition in this codebase uses (see its own doc
            // comment); this call site simply never adopted it.
            foreign_enter_idle(&shared);
        }
        if self.counted {
            FOREIGN_CALL_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
}

/// Idle -> running for this OS thread's foreign attachment to `shared`: leave
/// the blocked region (waiting out any pause in progress), then apply the
/// fixups every collection the idle window slept through accumulated, to the
/// thread's persistent fields and its JNI local frames, and re-deposit
/// (`check_post_block_gc`). See `ForeignCallGuard::enter` for why it is these
/// two canonical calls and nothing hand-rolled.
fn foreign_leave_idle(shared: &SharedVm) {
    shared.mem.gc_barrier.mark_blocked_region_leave();
    with_foreign_thread(|jt| {
        // gcd d3/k (`gcd-d2i-foreign-attached-threads-publish-no-os-tid`,
        // with `CRATONVM_JNI_FOREIGN_TRANSITIONS`): publish this OS thread's
        // id for as long as the attachment runs Java or a JNI function, so
        // the take-over can freeze and scan it in compiled code and a block
        // inside a callback is in the helper-window roster, like every other
        // Java thread. Before the flag clears (no pause is in progress here:
        // the leave above waited it out), so the first pause that counts the
        // thread also lists it.
        if foreign_transitions_active() {
            shared
                .threads
                .thread_registry
                .republish_os_tid_current(jt.thread_id);
        }
        crate::vm::NativeContextImpl { shared, thread: jt }.check_post_block_gc();
    });
}

/// Running -> idle for this OS thread's foreign attachment to `shared`: retire
/// the TLAB while still a counted mutator (a pause requested now is waiting
/// for this thread, so the tail filler cannot race its collector), deposit a
/// REAL root snapshot (frames, `java_thread_obj`, the JNI local frames, ...),
/// raise `in_blocked_region`, then re-enter the blocked region. See
/// `ForeignCallGuard`'s `Drop` for the history of each step.
fn foreign_enter_idle(shared: &SharedVm) {
    with_foreign_thread(|jt| {
        // gcd d3/k: withdraw the OS tid `foreign_leave_idle` published, while
        // still a counted mutator in Rust code: a pause waiting for us now
        // gets our cooperative arrival below, and an idle attachment has no
        // frame any pass could read. A no-op store when nothing was published.
        if foreign_transitions_active() {
            shared.threads.thread_registry.clear_os_tid(jt.thread_id);
        }
        jt.tlab.retire();
        crate::vm::NativeContextImpl { shared, thread: jt }.deposit_root_snapshot();
        jt.gc_block_state
            .in_blocked_region
            .store(true, std::sync::atomic::Ordering::Release);
    });
    let pre_stw = shared.mem.gc_barrier.mark_blocked_region_enter();
    if pre_stw {
        // GCAUDIT-0711-FIX (finding 1a): the in_blocked_region store above
        // already ran, so this pause may already have excluded us - auto
        // resolves it from that pause's own exclusion snapshot instead of
        // assuming participation. gc-common w2-c: no `ThreadId(0)` placeholder
        // -- main's real id (see `jni_caller_thread_id`). `None` is only
        // reachable with this thread's TLS already torn down, where there is
        // no identity to arrive under; not arriving is the hang-not-corruption
        // choice.
        if let Some(tid) = with_foreign_thread(|jt| jt.thread_id) {
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-function transition for an idle foreign thread
// (`CRATONVM_JNI_FOREIGN_TRANSITIONS`, default OFF)
// ---------------------------------------------------------------------------
//
// gc-common w10-c, `docs/known-issues/gc/common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`.
//
// A foreign-attached thread is GC-blocked while idle, and only a Java call
// (`ForeignCallGuard`) turned it into a counted mutator. Every other JNI
// function ran GC-blocked: `NewStringUTF` allocated, `SetByteArrayRegion`
// stored and `GetObjectField` read while another thread's stop-the-world
// collection copied, evacuated or relocated the heap; and what such a function
// created was rooted by nothing (no open frame; the deposited snapshot
// predated it), so a collection before the next Java call freed or moved it
// under the native.
//
// With the flag on, the two halves of HotSpot's model:
//
// * every JNIEnv function that touches the heap or a handle opens a
//   `ForeignJniEntry` first. On a foreign thread at depth 0 it is the
//   idle -> running -> idle round trip of `ForeignCallGuard` without the
//   per-call frame: it waits out a pause in progress, applies the fixups the
//   idle window accumulated (the JNI local frames included), and on exit
//   retires the TLAB, deposits, and goes idle again. Nested (inside a Java
//   call, or inside another function) and on every other thread it is inert;
//   the non-foreign cost is one thread-local read, only with the flag on.
//   The guard is the first statement of the function, before any handle is
//   decoded: an `ObjectRef` decoded while idle is stale after the leave's
//   remap. The `*MethodV` wrappers carry none: each delegates to its
//   `*MethodA` twin, which does;
// * an attach-level local frame ([`FOREIGN_ATTACH_FRAME`], opened by
//   `attach_foreign_thread`, closed by `detach_foreign_thread`). A local
//   created outside a Java call lives there until `DeleteLocalRef` or detach
//   (HotSpot's top-level handle block), is published by the deposit at the end
//   of the function that created it, and is remapped by the next leave. A
//   Java call made from the attach level runs nested inside its function's
//   entry, so its results land there too.
//
// Why a flag: it changes the thread-state protocol of every attached thread.
// Default it on after the `foreign_attach_soak` (`--cfg foreign_attach_soak`)
// and a JNA callback run are clean with it.

/// `CRATONVM_JNI_FOREIGN_TRANSITIONS`, latched like every flag. Default OFF;
/// the value rule is [`indirect_locals_from`]'s (`CRATONVM_GC=jni-foreign-transitions`
/// turns it on).
///
/// gcd d10/j: resolved with the other two JNI switches as ONE package
/// ([`jni_switches`]): on with `CRATONVM_JNI_NATIVE_TRANSITIONS` unless set to
/// `0`, and NEVER on without indirect locals -- an attached thread whose
/// transitions run while its locals are raw addresses is the configuration
/// that read a decommitted span after a move (`jniroots_C_1`, rc 139,
/// `common-w2c-jni-local-refs-are-raw-addresses.md`).
fn foreign_transitions_active() -> bool {
    if let Some(on) = foreign_transitions_test_override() {
        return on;
    }
    jni_switches().foreign_transitions
}

/// gcd d10/j: the three JNI in-native switches, resolved together once per
/// process (every flag is latched), so the combinations the round measured
/// are the only ones that can be selected.
///
/// * `CRATONVM_JNI_NATIVE_TRANSITIONS` (token `jni-native-transitions`) is the
///   PACKAGE switch: set on, it turns the other two on as well, unless one of
///   them is explicitly set off (`0` / `false` / `off` / `no`);
/// * `CRATONVM_JNI_INDIRECT_LOCALS` and `CRATONVM_JNI_FOREIGN_TRANSITIONS`
///   still work alone, as before (arms B and D of `Gcd1JniRootsProbe`);
/// * the foreign transitions are refused without indirect locals (arm C: an
///   attached thread whose JNI calls transition while its locals stay raw;
///   one run in three crashed on a raw local read after a move). A refused
///   `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` is reported once on stderr.
///
/// The native transitions are inert without indirect locals on their own
/// ([`locals_are_indirect_here`]), so `native` needs no such rule. All three
/// unset -- the default -- is all three OFF, byte-for-byte as before.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct JniSwitches {
    indirect_locals: bool,
    foreign_transitions: bool,
    native_transitions: bool,
}

/// The package rule of [`JniSwitches`], pure: each argument is a flag's
/// explicit value ([`jni_switch_value`]), `None` when it is unset.
fn jni_switches_from(
    native: Option<bool>,
    indirect: Option<bool>,
    foreign: Option<bool>,
) -> JniSwitches {
    let native_transitions = native.unwrap_or(false);
    let indirect_locals = indirect.unwrap_or(native_transitions);
    let foreign_transitions = foreign.unwrap_or(native_transitions) && indirect_locals;
    JniSwitches {
        indirect_locals,
        foreign_transitions,
        native_transitions,
    }
}

/// A JNI switch's explicit value: `None` when the variable is unset or empty,
/// otherwise [`indirect_locals_from`]'s rule (an off word is `Some(false)`).
fn jni_switch_value(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        None | Some("") => None,
        set => Some(indirect_locals_from(set)),
    }
}

/// The latched [`JniSwitches`] of this process.
fn jni_switches() -> JniSwitches {
    static SWITCHES: std::sync::OnceLock<JniSwitches> = std::sync::OnceLock::new();
    *SWITCHES.get_or_init(|| {
        let read = |name: &str| {
            jni_switch_value(cratonvm_types::flags::runtime_var(name).ok().as_deref())
        };
        let foreign = read("CRATONVM_JNI_FOREIGN_TRANSITIONS");
        let switches = jni_switches_from(
            read("CRATONVM_JNI_NATIVE_TRANSITIONS"),
            read("CRATONVM_JNI_INDIRECT_LOCALS"),
            foreign,
        );
        if foreign == Some(true) && !switches.foreign_transitions {
            eprintln!(
                "{}",
                concat!(
                    "[cratonvm] CRATONVM_JNI_FOREIGN_TRANSITIONS is ignored without ",
                    "CRATONVM_JNI_INDIRECT_LOCALS: an attached thread's raw local refs ",
                    "would go stale across a moving collection. Set ",
                    "CRATONVM_JNI_NATIVE_TRANSITIONS=1 for the whole JNI in-native package."
                )
            );
        }
        switches
    })
}

#[cfg(test)]
thread_local! {
    /// Unit tests switch the transitions per thread: the flag is latched per
    /// process, and tests share one.
    static FOREIGN_TRANSITIONS_TEST_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

#[cfg(test)]
fn foreign_transitions_test_override() -> Option<bool> {
    FOREIGN_TRANSITIONS_TEST_OVERRIDE
        .try_with(Cell::get)
        .ok()
        .flatten()
}

#[cfg(not(test))]
#[inline(always)]
fn foreign_transitions_test_override() -> Option<bool> {
    None
}

/// The per-function transition of an idle foreign thread; see the section
/// comment above. Bind it as the FIRST statement of a JNI function. The C
/// `jvmtiEnv` functions that decode a handle bind it too
/// (`jvmti::native_env`, interpreter round i1 wave 19).
///
/// gcd d3/k (`CRATONVM_JNI_NATIVE_TRANSITIONS`): the same entry is the
/// native -> VM -> native round trip of a thread whose innermost frame is a
/// JNI native method running in native ([`JniNativeCall`]); see the section
/// "A Java thread inside a JNI native method" below.
#[must_use = "the thread goes idle again when the entry drops"]
pub(crate) struct ForeignJniEntry {
    /// `Some(vm)` iff this entry left the idle region and must re-enter it.
    transition: Option<Arc<SharedVm>>,
    /// `Some` iff this entry took an in-native JNI native call out of native
    /// ([`JniNativeCall`]) and must put it back when the function returns.
    native: Option<(Arc<SharedVm>, *mut JvmThread)>,
    /// gcd d5/f: this entry opened a JNI leaf window
    /// ([`ForeignJniEntry::enter_leaf`]) and must close it -- or, if the
    /// function escalated to the full transition, put the call back in native.
    leaf: bool,
    /// gcd d5/f: this entry took the thread out of a GC-safe FFM downcall's
    /// region (`CRATONVM_FFM_DOWNCALL_GC_SAFE`: C code inside the downcall
    /// called a JNIEnv function) and must put it back.
    downcall: bool,
    /// gcd d10/j: this entry took an in-native call out of native QUIETLY for
    /// a function that runs no Java ([`ForeignJniEntry::enter_vm_only`]) and
    /// owns this thread's [`LEAF_VM_ONLY`] state: its drop may put the call
    /// back in native incrementally ([`native_call_reenter_incremental`]).
    vm_only: bool,
    /// gcd d10/j: with `vm_only`, the local frames' extent when the call left
    /// native: the locals past it are what the function minted.
    locals_mark: LocalFramesMark,
}

impl ForeignJniEntry {
    /// An entry that does nothing on drop.
    #[inline]
    fn inert() -> Self {
        ForeignJniEntry {
            transition: None,
            native: None,
            leaf: false,
            downcall: false,
            vm_only: false,
            locals_mark: LocalFramesMark {
                depth: 0,
                top_len: 0,
            },
        }
    }

    #[inline]
    pub(crate) fn enter() -> Self {
        // The innermost state first: a foreign-attached thread inside a Java
        // call can be in a JNI native too, and then it is that native call's
        // state this function must leave.
        if native_transitions_active() {
            if let Some(entry) = Self::enter_from_native(false) {
                return entry;
            }
        }
        Self::enter_outside_native()
    }

    /// gcd d10/j: [`Self::enter`] for a JNIEnv function whose ordinary path
    /// runs no Java, blocks on nothing and cannot collect: it allocates
    /// without a collection (`jni_alloc_or_oom`), reads a field or an element,
    /// copies a string out, or mints a local. Such a function changes none of
    /// the thread's frames, compiled frames or stashed deopt frames, so when
    /// the call left native quietly (no pause completed and nothing was folded
    /// into the thread since its deposit) the snapshot deposited on the way in
    /// still describes them, and going back into native needs only what the
    /// function can have added: the locals it minted, the off-frame roots (a
    /// pending exception), the SATB entries it logged
    /// ([`native_call_reenter_incremental`]) -- not a second full deposit
    /// (`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry`,
    /// section 2).
    ///
    /// Every way such a function can reach Java -- building an exception,
    /// `OutOfMemoryError` included -- goes through [`with_jni_context`], which
    /// marks the entry escalated ([`LEAF_VM_ONLY_ESCALATED`]); an escalated,
    /// non-quiet or re-synced entry goes back into native the full way. With
    /// the native transitions off this is exactly [`Self::enter`].
    #[inline]
    pub(crate) fn enter_vm_only() -> Self {
        if native_transitions_active() {
            if let Some(entry) = Self::enter_from_native(true) {
                return entry;
            }
        }
        Self::enter_outside_native()
    }

    /// [`Self::enter`] past the in-native check: the FFM downcall region, then
    /// the idle foreign attachment.
    #[inline]
    fn enter_outside_native() -> Self {
        // gcd d5/f: C code inside a GC-safe FFM downcall
        // (`CRATONVM_FFM_DOWNCALL_GC_SAFE`) that calls a JNIEnv function runs
        // excluded from every pause, like the C code itself: leave the
        // downcall's region first, as an upcall does, and go back on return.
        // One thread-local read when no GC-safe downcall region is open (the
        // flag off: always).
        if cratonvm_native_builtins::panama_libffi::leave_active_downcall_region() {
            // (No struct-update syntax: the type implements `Drop`.)
            let mut entry = Self::inert();
            entry.downcall = true;
            return entry;
        }
        if !foreign_transitions_active() || !is_foreign_attached() {
            return Self::inert();
        }
        Self::enter_foreign()
    }

    /// gcd d5/f: [`Self::enter`] for a LEAF JNIEnv function -- one that only
    /// reads or writes primitive contents of the object `handle` names (or,
    /// for `DeleteLocalRef`, only this thread's local-handle table): no
    /// allocation, no Java, no new reference, no blocking, no lock a pausing
    /// thread can hold. `handle` is the function's object argument.
    ///
    /// Inside a JNI native in native, when `handle` is one of this thread's
    /// indirect locals (or 0) and nothing moved since the native went into
    /// native, the function runs in a leaf window
    /// (`threading::gc_barrier::LeafWindowWord`) instead of the full native
    /// -> VM -> native round trip: the thread stays excluded from pauses and
    /// its deposited snapshot stays its roots; the window only proves no pause
    /// runs while it touches the heap. Two fences instead of ~8 us. Anything
    /// else -- a global ref, a `jclass`, a pause in progress, a relocation
    /// since the deposit -- takes [`Self::enter`]. A leaf function that must
    /// raise (an out-of-range region) escalates to the full transition before
    /// it builds the exception ([`escalate_leaf_window`], called by
    /// [`with_jni_context`]); by then it holds no decoded reference.
    ///
    /// gcd d10/j: a leaf function runs no Java on its ordinary path, so when
    /// no window can open it takes [`Self::enter_vm_only`] (a quiet leave and
    /// an incremental re-entry) rather than [`Self::enter`].
    #[inline]
    pub(crate) fn enter_leaf(handle: JObject) -> Self {
        if native_transitions_active() {
            if let Some(entry) = Self::enter_leaf_window(handle) {
                return entry;
            }
        }
        Self::enter_vm_only()
    }

    #[inline(never)]
    fn enter_leaf_window(handle: JObject) -> Option<Self> {
        // Only a handle this thread's own local frames resolve: a global ref
        // takes the global table's lock and a weak one an SATB enqueue, a
        // `jclass` the class tables -- none of which a leaf window may wait on.
        if handle != 0 && decode_indirect_local(handle).is_none() {
            return None;
        }
        // The blocked-access detector reports any heap access by a thread
        // whose `in_blocked_region` is up, which a leaf window is by design.
        if cratonvm_gc::blocked_access_debug::enabled() {
            return None;
        }
        let opened = JNI_NATIVE_CALL
            .try_with(|t| {
                if t.leaf.get() != LEAF_NONE {
                    return false;
                }
                let Ok(slot) = t.record.try_borrow() else {
                    return false;
                };
                let Some(rec) = slot.as_ref().filter(|r| r.in_native) else {
                    return false;
                };
                let word = t
                    .leaf_word
                    .get_or_init(|| Arc::new(crate::threading::gc_barrier::LeafWindowWord::new()));
                if !rec.shared.mem.gc_barrier.try_open_leaf_window(word) {
                    return false;
                }
                // No pause can begin collecting now. Did one complete, or
                // fold into this thread, since the deposit? Then the local
                // table still names pre-move addresses: the full transition.
                // SAFETY: see `NativeCallRecord::block_state`.
                if NativeSync::read(&rec.shared, unsafe { &*rec.block_state }) != rec.sync {
                    word.close();
                    return false;
                }
                t.leaf.set(LEAF_OPEN);
                true
            })
            .unwrap_or(false);
        // Built only when opened: a dropped entry with `leaf` set runs the
        // window's close in its `Drop`.
        if !opened {
            return None;
        }
        let mut entry = Self::inert();
        entry.leaf = true;
        Some(entry)
    }

    /// Native -> VM for a thread whose [`JNI_NATIVE_CALL`] record says it is
    /// in native: waits out a pause in progress and applies what the in-native
    /// window accumulated (`native_call_leave_native`). `None` (inert) when
    /// there is no record, or the record is not in native (a nested function,
    /// or a call kept counted).
    ///
    /// gcd d10/j: `vm_only` (from [`Self::enter_vm_only`]) keeps the
    /// deposit's `slot_origins` on a quiet leave and claims this thread's
    /// [`LEAF_VM_ONLY`] state -- only when no other entry holds the state, so
    /// an up-call's nested native never takes over an enclosing window's or
    /// entry's state -- which lets the drop re-enter incrementally.
    #[inline(never)]
    fn enter_from_native(vm_only: bool) -> Option<Self> {
        let (shared, thread, sync) = JNI_NATIVE_CALL
            .try_with(|t| {
                let mut slot = t.record.try_borrow_mut().ok()?;
                let rec = slot.as_mut().filter(|r| r.in_native)?;
                rec.in_native = false;
                Some((Arc::clone(&rec.shared), rec.thread, rec.sync))
            })
            .ok()
            .flatten()?;
        // SAFETY: `thread` is the `JvmThread` the dispatch that installed the
        // record runs on; it outlives the record (`JniNativeCall`'s drop
        // removes it before the dispatch returns), and no other `&mut` to it
        // is used while this one lives: the native is in C, and this function
        // body has not started.
        let quiet = native_call_leave_native(&shared, unsafe { &mut *thread }, sync, vm_only);
        let owns_vm_only = vm_only
            && quiet
            && JNI_NATIVE_CALL
                .try_with(|t| {
                    if t.leaf.get() != LEAF_NONE {
                        return false;
                    }
                    t.leaf.set(LEAF_VM_ONLY);
                    true
                })
                .unwrap_or(false);
        if vm_only && quiet && !owns_vm_only {
            // The origins the quiet leave kept are nobody's now: the drop
            // re-enters the full way, whose deposit records a fresh set.
            // SAFETY: as above.
            unsafe { &*thread }.gc_block_state.slot_origins.lock().clear();
        }
        let mut entry = Self::inert();
        entry.native = Some((shared, thread));
        entry.vm_only = owns_vm_only;
        if owns_vm_only {
            entry.locals_mark = local_frames_mark();
        }
        Some(entry)
    }

    #[inline(never)]
    fn enter_foreign() -> Self {
        let outermost = FOREIGN_CALL_DEPTH
            .try_with(|c| {
                if c.get() != 0 {
                    return false;
                }
                c.set(1);
                true
            })
            .unwrap_or(false);
        if !outermost {
            // Inside a Java call (or another JNI function): already a counted
            // mutator, and its frames scope what this function creates.
            return Self::inert();
        }
        let Some(shared) = foreign_attachment_vm() else {
            let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
            return Self::inert();
        };
        foreign_leave_idle(&shared);
        let mut entry = Self::inert();
        entry.transition = Some(shared);
        entry
    }
}

impl Drop for ForeignJniEntry {
    fn drop(&mut self) {
        if self.leaf {
            // gcd d5/f: close the leaf window; or, when the function
            // escalated (`escalate_leaf_window`), the call is out of native
            // now and goes back exactly as a full entry's does, below.
            let state = JNI_NATIVE_CALL
                .try_with(|t| {
                    let state = t.leaf.replace(LEAF_NONE);
                    if state == LEAF_OPEN {
                        if let Some(word) = t.leaf_word.get() {
                            word.close();
                        }
                    }
                    state
                })
                .unwrap_or(LEAF_NONE);
            if state == LEAF_ESCALATED {
                self.native = JNI_NATIVE_CALL
                    .try_with(|t| {
                        t.record.try_borrow().ok().and_then(|slot| {
                            slot.as_ref().map(|r| (Arc::clone(&r.shared), r.thread))
                        })
                    })
                    .ok()
                    .flatten();
            }
        }
        if self.downcall {
            // gcd d5/f: back into the FFM downcall's GC-safe region.
            self.downcall = false;
            cratonvm_native_builtins::panama_libffi::reenter_active_downcall_region();
        }
        if let Some(shared) = self.transition.take() {
            foreign_enter_idle(&shared);
            let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
        }
        if let Some((shared, thread)) = self.native.take() {
            // gcd d10/j: give back this entry's `LEAF_VM_ONLY` state and learn
            // whether the function stayed off every path to Java
            // (`with_jni_context` escalates it).
            let owned_vm_only = std::mem::take(&mut self.vm_only);
            let vm_only_clean = owned_vm_only
                && JNI_NATIVE_CALL
                    .try_with(|t| {
                        let state = t.leaf.get();
                        if state == LEAF_VM_ONLY || state == LEAF_VM_ONLY_ESCALATED {
                            t.leaf.set(LEAF_NONE);
                        }
                        state == LEAF_VM_ONLY
                    })
                    .unwrap_or(false);
            // SAFETY: as in `enter_from_native`; the function body's own
            // borrows of the thread ended with the body. A `Copy` field read.
            let redefinitions_now = unsafe { (*thread).redefinitions_seen };
            // Back into native, unless the call must stay counted from here
            // on: a raw (unencodable) local was handed out while it ran
            // (`raw_local_escapes`), which a moving collection in the native
            // window would leave stale in the native's own copy.
            let reenter = JNI_NATIVE_CALL
                .try_with(|t| {
                    let mut slot = t.record.try_borrow_mut().ok()?;
                    let rec = slot.as_mut()?;
                    if rec.thread != thread || rec.in_native {
                        return None;
                    }
                    if t.raw_escapes.get() != rec.raw_escapes {
                        return None;
                    }
                    rec.in_native = true;
                    // gcd d5/f: read before the deposit below, so a pause
                    // that completes during it counts as "moved since".
                    // SAFETY: see `NativeCallRecord::block_state`.
                    let fresh = NativeSync::read(&rec.shared, unsafe { &*rec.block_state });
                    // gcd d10/j: the deposit made on the way into native still
                    // describes the frames when the function ran no Java, the
                    // leave was quiet, no pause completed and nothing was
                    // folded since (`fresh == sync`), no frame was converted
                    // for a redefinition, and the appends are still few.
                    let incremental = vm_only_clean
                        && fresh == rec.sync
                        && rec.deposit_redefinitions == redefinitions_now
                        && rec.appended < INCREMENTAL_APPEND_LIMIT;
                    if !incremental {
                        rec.sync = fresh;
                        rec.deposit_redefinitions = redefinitions_now;
                        rec.appended = 0;
                    }
                    Some(incremental)
                })
                .ok()
                .flatten();
            match reenter {
                Some(true) => {
                    // SAFETY: as above.
                    let added = native_call_reenter_incremental(
                        &shared,
                        unsafe { &mut *thread },
                        self.locals_mark,
                    );
                    let _ = JNI_NATIVE_CALL.try_with(|t| {
                        if let Ok(mut slot) = t.record.try_borrow_mut() {
                            if let Some(rec) = slot.as_mut().filter(|r| r.thread == thread) {
                                rec.appended = rec.appended.saturating_add(added);
                            }
                        }
                    });
                }
                Some(false) => {
                    // SAFETY: as above.
                    native_call_enter_native(&shared, unsafe { &mut *thread });
                }
                None => {
                    if owned_vm_only {
                        // The call stays counted: the origins the quiet leave
                        // kept describe no blocked window any more.
                        // SAFETY: as above; the lock is the field's own.
                        unsafe { &*thread }.gc_block_state.slot_origins.lock().clear();
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A Java thread inside a JNI native method (`CRATONVM_JNI_NATIVE_TRANSITIONS`,
// default OFF)
// ---------------------------------------------------------------------------
//
// gcd d3/k, `docs/known-issues/gc/gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`.
//
// HotSpot runs a JNI native method in `_thread_in_native`: safepoint-SAFE for
// the whole call, so a native that blocks in C (`pthread_join`, `read`,
// `poll`, a JNA call of any blocking function) holds no pause. Here a Java
// thread inside a native was a COUNTED mutator for the whole call, and a
// pause requested meanwhile waited for the C call to return -- forever, when
// the C call waited on something the parked Java threads would have done.
//
// With the flag on, `dispatch_jni_native` brackets the C call in a
// [`JniNativeCall`]:
//
// * running -> in native, before the C call (`native_call_enter_native`):
//   Thread.getState() stays RUNNABLE (`java_state` 0, as HotSpot reports a
//   thread in native), the TLAB is retired, the root snapshot deposited -- the
//   thread's interpreter frames, its compiled frames' conservative roots and
//   pins, its native pins (the dispatch's pinned arguments, a synchronized
//   native's monitor), and its JNI local frames, the native's receiver and
//   arguments included -- `in_blocked_region` raised, and the blocked region
//   entered (arriving if a pause raced in). From here every pause excludes
//   the thread and folds its moves into the thread's fixup, exactly as for a
//   thread blocked in `Object.wait`;
// * every JNIEnv function the native calls opens a `ForeignJniEntry`, which
//   takes the thread back out of native for the function (waiting out a pause
//   in progress, applying the fixups -- the JNI local frames included) and
//   puts it back when the function returns. Up-calls run Java as a counted
//   mutator inside that window; a native that Java calls in turn brackets its
//   own C call, and restores the enclosing record on return;
// * in native -> running, after the C call and before its result is decoded
//   (`native_call_leave_native`).
//
// WHY INDIRECT LOCALS ARE REQUIRED. The collector rewrites the thread's local
// frame TABLE on the leave, never the native's own copies of the handles. With
// the default raw-address locals a moving collection in the native window
// would leave every `jobject` the native holds naming from-space: the
// use-after-move of `common-w2c-jni-local-refs-are-raw-addresses.md`, widened
// from "an up-call or contended MonitorEnter inside the native" to "any
// collection by any thread while the native runs". Making the window
// non-moving instead is not expressible on every backend (a Generational young
// copy cannot honour a per-object pin; it can only refuse the whole cycle), so
// the transition is taken ONLY where every local the native can hold is an
// indirect `(frame, slot)` handle: `CRATONVM_JNI_INDIRECT_LOCALS` on, and on a
// foreign-attached thread `CRATONVM_JNI_FOREIGN_TRANSITIONS` too
// (`locals_are_indirect_here`, the rule `record_local_handle_indirect` uses).
// Otherwise the call stays counted, exactly as before. A raw handle can still
// escape with both on (a slot past the encodable range, no open frame): each
// one bumps `raw_local_escapes`, a native call whose receiver or argument
// escaped is never taken into native, and one whose JNIEnv function hands one
// out stays counted from that function on.
//
// What else the native can hold and why it is sound: global and weak-global
// refs are table indices resolved at every use; a `jclass` is a tagged class
// id; `Get<Type>ArrayElements`, `GetPrimitiveArrayCritical` and the string
// `Get*Chars` hand out detached copies written back through a global ref at
// Release; `GetDirectBufferAddress` is off-heap memory.
//
// Cost (why a flag): per JNI native call one deposit on entry and one fixup
// application plus re-deposit on exit, and the same pair around EVERY JNIEnv
// function the native calls -- a `Get<Type>ArrayRegion` loop pays it per
// element call. HotSpot's transitions are two stores and a fence.
//
// What is left of that cost after gcd d5/f and d10/j:
// * a LEAF function (`ForeignJniEntry::enter_leaf`) runs in a leaf window: two
//   fences, the thread never leaves native;
// * a function that runs no Java (`ForeignJniEntry::enter_vm_only`:
//   `NewStringUTF`, `New<Prim>Array`, `GetObjectField`, ...) leaves quietly
//   (one barrier-lock hold, no wake) and re-enters incrementally (append its
//   new locals and off-frame roots, one barrier-lock hold, no deposit);
// * every other function, and every native CALL, still pays one full
//   `deposit_root_snapshot` on the way into native -- the per-call floor of
//   `Gcd1JniCostProbe`'s `noop` on the flag arm, which only a lighter
//   in-native deposit (`vm_exec.rs`, not this file) removes; see
//   `docs/known-issues/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`.
// No bracket publishes the code-reclamation blocked-stack summary any more
// (`GcBarrier::mark_in_native_enter`): a thread in native holds retired
// compiled bodies as it does with the flag off.

/// `CRATONVM_JNI_NATIVE_TRANSITIONS`, latched like every flag. Default OFF;
/// the value rule is [`indirect_locals_from`]'s (`CRATONVM_GC=jni-native-transitions`
/// turns it on). Inert unless locals are indirect ([`locals_are_indirect_here`]).
/// gcd d10/j: it is also the package switch of the other two JNI flags
/// ([`jni_switches`]).
fn native_transitions_active() -> bool {
    if let Some(on) = native_transitions_test_override() {
        return on;
    }
    jni_switches().native_transitions
}

/// Unit tests switch the native transitions per thread (the flag is latched
/// per process): [`NativeCallTls::test_override`].
#[cfg(test)]
fn native_transitions_test_override() -> Option<bool> {
    JNI_NATIVE_CALL
        .try_with(|t| t.test_override.get())
        .ok()
        .flatten()
}

#[cfg(not(test))]
#[inline(always)]
fn native_transitions_test_override() -> Option<bool> {
    None
}

/// Is every local handed to native code on this thread an indirect handle?
/// The rule [`record_local_handle_indirect`] encodes by.
fn locals_are_indirect_here() -> bool {
    indirect_locals_active() && (!is_foreign_attached() || foreign_transitions_active())
}

/// How many local refs this thread handed to native code RAW although
/// [`locals_are_indirect_here`] held (no open frame, or a slot past the
/// encodable range). Monotonic; compared, never reset. `pub(crate)` for a
/// caller that brackets a C call outside `dispatch_jni_native` in a
/// [`JniNativeCall`] (`docs/internal/gc/gcd-d4k-jni-onload-runs-counted-FIXED-20260928.md`).
pub(crate) fn raw_local_escapes() -> u64 {
    JNI_NATIVE_CALL
        .try_with(|t| t.raw_escapes.get())
        .unwrap_or(0)
}

/// Is this thread inside a JNI native call that is in native right now
/// ([`JniNativeCall`])? Never with `CRATONVM_JNI_NATIVE_TRANSITIONS` off.
fn in_native_jni_call() -> bool {
    JNI_NATIVE_CALL
        .try_with(|t| {
            t.record
                .try_borrow()
                .is_ok_and(|r| r.as_ref().is_some_and(|r| r.in_native))
        })
        .unwrap_or(false)
}

/// Count one raw escape (see [`raw_local_escapes`]).
///
/// gcd d4/k2: inside a native dispatch that was counted as holding only
/// indirect locals ([`RawLocalsDispatch`]), the native now holds a raw one the
/// dispatch's scope does not cover: open this thread's raw-locals count in the
/// JNI context's VM for good, once per thread (fail closed; see the section
/// "The raw-JNI-locals count").
fn note_raw_local_escape() {
    let late = JNI_NATIVE_CALL
        .try_with(|t| {
            t.raw_escapes.set(t.raw_escapes.get().wrapping_add(1));
            if t.uncounted_dispatches.get() > 0 && !t.late_raw_opened.get() {
                t.late_raw_opened.set(true);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if late {
        let _ = with_shared_vm(|shared| {
            cratonvm_gc::gc_quiescence::note_raw_jni_locals_open(
                shared.mem.gc_barrier.pause_ledger(),
            );
        });
    }
}

/// The in-native record of the innermost JNI native call on this thread that
/// [`JniNativeCall`] took into native.
struct NativeCallRecord {
    /// The VM the call runs in (the dispatch's context; kept here because a
    /// `DetachCurrentThread` from the native clears the JNI context).
    shared: Arc<SharedVm>,
    /// The dispatching thread's `JvmThread` (the dispatch's `JNI_THREAD`).
    thread: *mut JvmThread,
    /// `true` while the thread is in native (GC-blocked); `false` while a
    /// JNIEnv function runs, or for good once a raw local escaped.
    in_native: bool,
    /// [`raw_local_escapes`] when the call went into native.
    raw_escapes: u64,
    /// gcd d5/f: the dispatching thread's `gc_block_state`
    /// (`Arc::as_ptr(&thread.gc_block_state)`), valid for the record's whole
    /// life: the `Arc` is owned by `*thread`, which outlives the record. A raw
    /// pointer so the leaf path reads its fold count without forming a
    /// reference to the `JvmThread` the suspended interpreter holds mutably.
    block_state: *const crate::threading::jvm_thread::GcBlockState,
    /// gcd d5/f: [`NativeSync`] read just before the call last went into
    /// native (its deposit). Equal to a fresh read = nothing this thread holds
    /// has moved since, and its deposited snapshot is still exact.
    sync: NativeSync,
    /// gcd d10/j: the thread's `redefinitions_seen` at the last FULL deposit.
    /// A quiet leave converts obsolete frames when a class was redefined
    /// (`convert_obsolete_frames_if_redefined` advances the field), after
    /// which that deposit no longer describes the frames.
    deposit_redefinitions: u64,
    /// gcd d10/j: snapshot entries [`native_call_reenter_incremental`] appended
    /// since the last full deposit; at [`INCREMENTAL_APPEND_LIMIT`] the next
    /// re-entry is a full deposit again, which drops what deleted locals kept.
    appended: u32,
}

/// gcd d10/j: how many snapshot entries incremental re-entries may append
/// before the next re-entry deposits in full. Each append keeps the referents
/// of every local of the call alive until that full deposit (over-retention
/// only, never a lost root); a `NewStringUTF` + `DeleteLocalRef` loop appends
/// one entry per round, so this is one full deposit per ~256 rounds.
const INCREMENTAL_APPEND_LIMIT: u32 = 256;

/// gcd d5/f: "has anything happened to this thread's roots since it went into
/// native?" as two counters: the VM's pause generation (any completed pause)
/// and the thread's fold count (any relocation folded into its blocked state,
/// `GcBlockState::blocked_folds`). Relocations run only inside pauses, and a
/// pause never completes without advancing the generation, so the fold count
/// is the belt to the generation's braces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct NativeSync {
    generation: u64,
    folds: u64,
}

impl NativeSync {
    #[inline]
    fn read(shared: &SharedVm, block: &crate::threading::jvm_thread::GcBlockState) -> Self {
        NativeSync {
            generation: shared
                .mem
                .gc_barrier
                .gc_generation
                .load(std::sync::atomic::Ordering::Acquire),
            folds: block
                .blocked_folds
                .load(std::sync::atomic::Ordering::Acquire),
        }
    }
}

/// [`NativeCallTls::leaf`]: no leaf window on this thread.
const LEAF_NONE: u8 = 0;
/// A leaf JNIEnv function runs in an open window.
const LEAF_OPEN: u8 = 1;
/// A leaf function escalated to the full transition
/// ([`escalate_leaf_window`]): the call is out of native until the
/// function's entry drops.
const LEAF_ESCALATED: u8 = 2;
/// gcd d10/j: a function entered through [`ForeignJniEntry::enter_vm_only`]
/// left native quietly and runs, counted, without having reached Java.
const LEAF_VM_ONLY: u8 = 3;
/// gcd d10/j: that function reached [`with_jni_context`] (it builds an
/// exception, or otherwise may run Java): its entry re-enters the full way.
const LEAF_VM_ONLY_ESCALATED: u8 = 4;

/// This thread's state for the in-native transitions: one thread-local (the
/// per-VM statics ratchet counts every declaration line).
struct NativeCallTls {
    /// See [`NativeCallRecord`]. `None` outside every in-native call.
    record: std::cell::RefCell<Option<NativeCallRecord>>,
    /// gcd d5/f: [`LEAF_NONE`] / [`LEAF_OPEN`] / [`LEAF_ESCALATED`]; see
    /// [`ForeignJniEntry::enter_leaf`]. gcd d10/j: also [`LEAF_VM_ONLY`] /
    /// [`LEAF_VM_ONLY_ESCALATED`] ([`ForeignJniEntry::enter_vm_only`]); at most
    /// one entry owns a non-`NONE` state at a time.
    leaf: Cell<u8>,
    /// gcd d5/f: this OS thread's leaf-window word, made at its first leaf
    /// window and registered with each barrier it opens one under. Marked
    /// dead when this thread-local is dropped (`Drop for NativeCallTls`).
    leaf_word: std::cell::OnceCell<Arc<crate::threading::gc_barrier::LeafWindowWord>>,
    /// See [`raw_local_escapes`].
    raw_escapes: Cell<u64>,
    /// gcd d4/k2: native dispatches on this thread in progress that were NOT
    /// counted as holding raw locals (their locals were indirect); see
    /// [`RawLocalsDispatch`].
    uncounted_dispatches: Cell<u32>,
    /// gcd d4/k2: this thread already opened its fail-closed raw-locals count
    /// for a raw escape inside an uncounted dispatch (see
    /// [`note_raw_local_escape`]).
    late_raw_opened: Cell<bool>,
    /// gcd d10/j (`gcd-d5f-jni-default-dispatch-costs-500ns`): the storage of
    /// the last local frame [`truncate_local_frames`] closed, emptied, kept
    /// for the next [`push_local_frame`] -- every JNI native dispatch opens and
    /// closes one implicit frame, which was a heap allocation and a free per
    /// call. Holds at most one `Vec` of at most [`SPARE_LOCAL_FRAME_MAX`]
    /// slots; never holds a handle (it is cleared before it is kept).
    spare_local_frame: Cell<Vec<JObject>>,
    /// gce e1/j: the last [`find_jni_native`] hit ([`JniFnMemo`]).
    fn_memo: std::cell::RefCell<Option<JniFnMemo>>,
    /// Unit tests' per-thread switch of [`native_transitions_active`].
    #[cfg(test)]
    test_override: Cell<Option<bool>>,
    /// Unit tests' per-thread switch of [`raw_locals_counted`].
    #[cfg(test)]
    raw_locals_override: Cell<Option<bool>>,
}

impl NativeCallTls {
    const fn new() -> Self {
        NativeCallTls {
            record: std::cell::RefCell::new(None),
            leaf: Cell::new(LEAF_NONE),
            leaf_word: std::cell::OnceCell::new(),
            raw_escapes: Cell::new(0),
            uncounted_dispatches: Cell::new(0),
            late_raw_opened: Cell::new(false),
            spare_local_frame: Cell::new(Vec::new()),
            fn_memo: std::cell::RefCell::new(None),
            #[cfg(test)]
            test_override: Cell::new(None),
            #[cfg(test)]
            raw_locals_override: Cell::new(None),
        }
    }
}

impl Drop for NativeCallTls {
    fn drop(&mut self) {
        // The OS thread is ending: every barrier this word is registered with
        // forgets it at its next registration, and no drain waits on it.
        if let Some(word) = self.leaf_word.get() {
            word.mark_owner_dropped();
        }
    }
}

/// gcd d5/f: called by [`with_jni_context`] -- the door every JNI path that
/// builds an object, raises an exception or runs Java takes. A leaf function
/// that reaches it (an out-of-range region raising
/// `ArrayIndexOutOfBoundsException`) must not do that inside its leaf window:
/// close the window and take the call out of native the full way first
/// (wait out a pause, apply the window's fixups). The function's entry puts
/// the call back in native when it drops. One thread-local read otherwise.
///
/// Sound because a leaf function reaches this only on its way to raising, after
/// its last use of the reference it decoded in the window.
///
/// gcd d10/j: a function entered through [`ForeignJniEntry::enter_vm_only`]
/// that gets here may run Java from now on (an exception's constructor, a
/// class initializer): it is marked [`LEAF_VM_ONLY_ESCALATED`], and its entry
/// goes back into native with a full deposit. The same one thread-local read.
#[inline]
fn escalate_leaf_window() {
    let open = JNI_NATIVE_CALL
        .try_with(|t| match t.leaf.get() {
            LEAF_OPEN => true,
            LEAF_VM_ONLY => {
                t.leaf.set(LEAF_VM_ONLY_ESCALATED);
                false
            }
            _ => false,
        })
        .unwrap_or(false);
    if open {
        escalate_leaf_window_slow();
    }
}

#[inline(never)]
fn escalate_leaf_window_slow() {
    let target = JNI_NATIVE_CALL
        .try_with(|t| {
            if let Some(word) = t.leaf_word.get() {
                word.close();
            }
            let mut slot = t.record.try_borrow_mut().ok();
            let rec = slot
                .as_deref_mut()
                .and_then(Option::as_mut)
                .filter(|r| r.in_native);
            match rec {
                Some(rec) => {
                    rec.in_native = false;
                    t.leaf.set(LEAF_ESCALATED);
                    Some((Arc::clone(&rec.shared), rec.thread, rec.sync))
                }
                None => {
                    // Not reachable (a window opens only on a record in
                    // native, and nothing else clears it meanwhile); the
                    // window is closed, nothing to put back.
                    t.leaf.set(LEAF_NONE);
                    None
                }
            }
        })
        .ok()
        .flatten();
    if let Some((shared, thread, sync)) = target {
        // SAFETY: as in `ForeignJniEntry::enter_from_native`: the record's
        // `JvmThread` outlives it, and the leaf function holds no `&mut` to it.
        let _ = native_call_leave_native(&shared, unsafe { &mut *thread }, sync, false);
    }
}

// ---------------------------------------------------------------------------
// The raw-JNI-locals count (gcd d4/k2, lane m's request 1)
// ---------------------------------------------------------------------------
//
// `cratonvm_gc::gc_quiescence::RawJniLocalsScope`: while any thread of a VM
// may hold a RAW local ref (an address its C code keeps and nothing can
// rewrite), the young pin ledger reads INCOMPLETE, so the opt-in pinned young
// copy / option B do not relocate under it. Fed from here:
//
// * every JNI native dispatch ([`dispatch_jni_native`], so both arms of the
//   `vm_exec.rs` JNI region by construction) holds a [`RawLocalsDispatch`]
//   from just after its receiver and arguments are recorded -- so a raw one
//   among them is known -- until after the C call returned and, with the
//   native transitions, left native. Counted iff [`local_refs_may_be_held_raw`]
//   (the default representation, or a raw escape on this thread). Nested
//   natives hold their own scopes, balanced by RAII;
// * a raw escape INSIDE a dispatch that was counted "indirect" (practically
//   unreachable: an implicit frame is always open, and a slot index past 2^31
//   or a frame depth past 65535 is needed) opens this thread's count for good
//   ([`note_raw_local_escape`], once per thread): fail closed, a lost
//   optimisation rather than a stale raw copy;
// * an `AttachCurrentThread` attachment with raw locals is counted from attach
//   to detach ([`count_attachment_raw_locals`], closed by its
//   `ForeignThreadBox`).

/// Does anything read the raw-JNI-locals count (`raw_jni_locals_matter`:
/// the pinned young copy or option B is on)? Unit tests switch it per thread.
fn raw_locals_counted() -> bool {
    #[cfg(test)]
    {
        if let Some(on) = JNI_NATIVE_CALL
            .try_with(|t| t.raw_locals_override.get())
            .ok()
            .flatten()
        {
            return on;
        }
    }
    cratonvm_gc::gc_quiescence::raw_jni_locals_matter()
}

/// The raw-JNI-locals count of one native dispatch (see the section above).
/// `pub(crate)` for the other places C code runs with a JNI context and may
/// hold raw locals: `JNI_OnLoad` (`vm_exec.rs`, the `System.load` path) and a
/// JVMTI agent's event callback (`jvmti::native_env::in_event_context`).
#[must_use = "the raw-local count drops when the scope drops"]
pub(crate) struct RawLocalsDispatch {
    _scope: Option<cratonvm_gc::gc_quiescence::RawJniLocalsScope>,
    /// This dispatch bumped [`NativeCallTls::uncounted_dispatches`].
    uncounted: bool,
}

impl RawLocalsDispatch {
    /// For the C call the JNI context (`JniContextGuard` /
    /// `replace_jni_context`) is installed for: counts it in that VM's pause
    /// ledger while [`local_refs_may_be_held_raw`]. Inert when nothing reads
    /// the count or no context is installed.
    pub(crate) fn enter() -> Self {
        let inert = RawLocalsDispatch {
            _scope: None,
            uncounted: false,
        };
        if !raw_locals_counted() {
            return inert;
        }
        let ledger = JNI_SHARED_VM
            .try_with(|c| {
                c.try_borrow().ok().and_then(|b| {
                    b.as_ref()
                        .map(|s| Arc::clone(s.mem.gc_barrier.pause_ledger()))
                })
            })
            .ok()
            .flatten();
        let Some(ledger) = ledger else {
            return inert;
        };
        let raw = local_refs_may_be_held_raw();
        let scope = cratonvm_gc::gc_quiescence::RawJniLocalsScope::enter(&ledger, raw);
        if !raw {
            let _ = JNI_NATIVE_CALL
                .try_with(|t| t.uncounted_dispatches.set(t.uncounted_dispatches.get() + 1));
        }
        RawLocalsDispatch {
            _scope: Some(scope),
            uncounted: !raw,
        }
    }
}

impl Drop for RawLocalsDispatch {
    fn drop(&mut self) {
        if self.uncounted {
            let _ = JNI_NATIVE_CALL.try_with(|t| {
                t.uncounted_dispatches
                    .set(t.uncounted_dispatches.get().saturating_sub(1))
            });
        }
    }
}

thread_local! {
    /// See [`NativeCallTls`].
    static JNI_NATIVE_CALL: NativeCallTls = const { NativeCallTls::new() };
}

// ---------------------------------------------------------------------------
// The JNI phase census (gce e1/j, `CRATONVM_DBG_JNI_PHASE=1`)
// ---------------------------------------------------------------------------

/// gce e1/j: one phase of a JNI native dispatch timed by [`JniPhaseCensus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JniPhase {
    /// `vm_exec`'s JNI door: `find_jni_native`.
    Lookup,
    /// `vm_exec`: `JniContextGuard::install_for_native`.
    CtxInstall,
    /// `vm_exec`: `JniImplicitFrameGuard::enter`.
    LocalFramePush,
    /// `vm_exec`: env, receiver, arity check, census counter, native frame.
    DoorPrologue,
    /// `vm_exec`: the whole `dispatch_jni_native` call.
    Dispatch,
    /// `vm_exec`: `jni_pending_exception_after_native`.
    ExceptionCheck,
    /// `dispatch_jni_native`: tags, receiver / argument handles, `jargs`.
    ArgMarshal,
    /// `dispatch_jni_native`: `RawLocalsDispatch::enter`.
    RawLocals,
    /// `JniNativeCall::enter_engaged`, whole (the package only).
    EnterNative,
    /// `native_call_enter_native`: TLAB retire.
    TlabRetire,
    /// `native_call_enter_native`: the whole deposit.
    Deposit,
    /// `native_call_enter_native`: `enter_in_native_region` (barrier).
    InNativeRegion,
    /// The C function itself (`call_jni_marshalled`).
    NativeBody,
    /// `JniNativeCall`'s drop, whole (the package only).
    LeaveNative,
    /// `native_call_leave_native`: `GcBarrier::leave_in_native_if`.
    LeaveBarrier,
    /// `native_call_leave_native`: quiet tail (origins, redefinition check).
    LeaveQuietTail,
    /// `native_call_leave_native`: the full wake (`check_post_block_gc`).
    LeaveFullWake,
    /// `dispatch_jni_native`: result decode.
    ResultDecode,
    /// Deposit (in-native only): publish marker, SATB flush, scan invalidation.
    DepPrologue,
    /// Deposit: `frame_class_owners`.
    DepClassOwners,
    /// Deposit: the interpreter frames' slots (the `snapshot` lock included).
    DepFrames,
    /// Deposit: stashed deopt frames and `slot_origins`.
    DepOrigins,
    /// Deposit: off-frame thread roots and the JNI local frames.
    DepOffFrame,
    /// Deposit: the moving-young coverage proof (Generational).
    DepCoverage,
    /// Deposit: the conservative JIT scan and `publish_pinned_jit_roots`.
    DepJitScan,
    /// Deposit: the shadow stack and the debug checks after it.
    DepShadow,
    /// Deposit: the reclaimed-slot audit and the flag.
    DepTail,
    /// Coverage proof (inside `dep_coverage`): gates, prune, `shadow_bail`.
    CovPrologue,
    /// Coverage proof: the per-entry chain walk (frame maps, parents).
    CovChainWalk,
    /// Coverage proof: the A5 block (memoised by the frozen band).
    CovA5,
    /// Coverage proof: `moving_young_unpublished_frame_oop_present`.
    CovBandVerify,
    /// gce e2/j: `scan_active_jit_frames`: the A5 block (unregistered-frame probe).
    JitA5,
    /// gce e2/j: `scan_active_jit_frames`: the chain's frames (`scan_active_jit_frames_with_sp`).
    JitChainFrames,
    /// gce e2/j: Deposit: `publish_pinned_jit_roots` and the G1 reject pins.
    DepJitPins,
    /// gce e2/j: Band verify: building the shadow-published value set.
    CovBandShadowSet,
    /// gce e2/j: Deposit: `capture_published_trace` and its store.
    TraceCapture,
    /// gce e2/j: Deposit: `publish_jmx_frame_monitors`.
    TraceJmx,
    /// gce e2/j: Deposit: `publish_blocked_frame_classes`.
    TraceCensus,
    /// gce e2/j: COUNT: A5 block answered by a raw-clean frozen-band verdict.
    A5MemoRaw,
    /// gce e2/j: COUNT: A5 block answered by a classified (residue-only) verdict.
    A5MemoClassified,
    /// gce e2/j: COUNT: A5 probe found no JIT return address.
    A5ProbeClean,
    /// gce e2/j: COUNT: A5 hit at or above the residue mark: accepted, band marked.
    A5AcceptLive,
    /// gce e2/j: COUNT: A5 hit with no residue mark (0): accepted, band marked.
    A5AcceptNoResidue,
    /// gce e2/j: COUNT: A5 hit accepted because the chain is non-empty (not judged).
    A5AcceptChain,
    /// gce e2/j: COUNT: A5 residue hit, but a live frame above the mark: rescued, marked.
    A5Rescued,
    /// gce e2/j: COUNT: A5 residue-only hit declined (nothing marked).
    A5Declined,
    /// gce e2/j: COUNT: a clean / declined verdict NOT memoised (partial or truncated band).
    A5NotRecorded,
    /// gce e2/j: COUNT: a precise chain entry scanned by its frame bands.
    FrameBands,
    /// gce e2/j: COUNT: a precise entry fell back to the whole-band scan: no usable exact RBP.
    FrameFallbackNoRbp,
    /// gce e2/j: COUNT: a precise entry fell back to the whole-band scan: bands refused.
    FrameFallbackBands,
    /// gce e2/j: COUNT: a conservative (map-less) chain entry: whole-band scan.
    FrameConservative,
    /// gce e2/j: COUNT: compiled frames the band verify walked.
    CovBandFrames,
    /// gce e2/j: COUNT (summed): words of those frames' bands.
    CovBandWords,
}

impl JniPhase {
    const COUNT: usize = JniPhase::CovBandWords as usize + 1;
    const NAMES: [&'static str; JniPhase::COUNT] = [
        "lookup",
        "ctx_install",
        "local_frame_push",
        "door_prologue",
        "dispatch",
        "exception_check",
        "arg_marshal",
        "raw_locals",
        "enter_native",
        "tlab_retire",
        "deposit",
        "in_native_region",
        "native_body",
        "leave_native",
        "leave_barrier",
        "leave_quiet_tail",
        "leave_full_wake",
        "result_decode",
        "dep_prologue",
        "dep_class_owners",
        "dep_frames",
        "dep_origins",
        "dep_off_frame",
        "dep_coverage",
        "dep_jit_scan",
        "dep_shadow",
        "dep_tail",
        "cov_prologue",
        "cov_chain_walk",
        "cov_a5",
        "cov_band_verify",
        "jit_a5",
        "jit_chain_frames",
        "dep_jit_pins",
        "cov_band_shadow_set",
        "trace_capture",
        "trace_jmx",
        "trace_census",
        "a5_memo_raw",
        "a5_memo_classified",
        "a5_probe_clean",
        "a5_accept_live",
        "a5_accept_no_residue",
        "a5_accept_chain",
        "a5_rescued",
        "a5_declined",
        "a5_not_recorded",
        "frame_bands",
        "frame_fallback_no_rbp",
        "frame_fallback_bands",
        "frame_conservative",
        "cov_band_frames",
        "cov_band_words",
    ];
}

/// gce e1/j: per-VM wall-time census of the phases of a JNI native dispatch
/// ([`JniPhase`]), on with `CRATONVM_DBG_JNI_PHASE=1` (read once, when the VM
/// is built: `NativeRealm::jni_phase_census`). Off, every probe is one load
/// of [`Self::on`] and a not-taken branch. On, each phase boundary is one
/// `Instant::now()` and two relaxed adds; the deposit's sub-phases are timed
/// only inside the package's in-native deposit. Printed at exit as one
/// `[GC] jni_phase:` line (`jni_phase_census_line`, from `vm-cli`).
pub struct JniPhaseCensus {
    on: bool,
    ns: [std::sync::atomic::AtomicU64; JniPhase::COUNT],
    hits: [std::sync::atomic::AtomicU64; JniPhase::COUNT],
}

impl JniPhaseCensus {
    /// Read `CRATONVM_DBG_JNI_PHASE` (once per VM).
    pub fn from_env() -> Self {
        Self::new(cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JNI_PHASE"))
    }

    pub(crate) fn new(on: bool) -> Self {
        JniPhaseCensus {
            on,
            ns: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            hits: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// A phase clock, or `None` when the census is off.
    /// Is the census on (the one `bool` every probe reads)?
    #[inline(always)]
    pub(crate) fn is_on(&self) -> bool {
        self.on
    }

    #[inline(always)]
    pub(crate) fn start(&self) -> Option<std::time::Instant> {
        if self.on {
            Some(std::time::Instant::now())
        } else {
            None
        }
    }

    /// Charge `phase` with the time since `clock` and restart the clock.
    #[inline(always)]
    pub(crate) fn lap(&self, phase: JniPhase, clock: &mut Option<std::time::Instant>) {
        if let Some(t0) = *clock {
            let now = std::time::Instant::now();
            self.add(phase, now.duration_since(t0));
            *clock = Some(now);
        }
    }

    /// gce e2/j: count one event of a `COUNT:` phase (no time).
    #[inline(always)]
    pub(crate) fn note(&self, phase: JniPhase) {
        if self.on {
            self.add(phase, std::time::Duration::ZERO);
        }
    }

    /// gce e2/j: add `n` to a `COUNT:` phase's count (no time). The line's
    /// `avg` is then 0 and the count is the sum.
    #[inline(always)]
    pub(crate) fn count(&self, phase: JniPhase, n: u64) {
        if self.on {
            self.hits[phase as usize].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Charge `phase` with the time since `clock`.
    #[inline(always)]
    pub(crate) fn stop(&self, phase: JniPhase, clock: Option<std::time::Instant>) {
        if let Some(t0) = clock {
            self.add(phase, t0.elapsed());
        }
    }

    #[cold]
    fn add(&self, phase: JniPhase, d: std::time::Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        let i = phase as usize;
        self.ns[i].fetch_add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX), Relaxed);
        self.hits[i].fetch_add(1, Relaxed);
    }

    /// The `[GC] jni_phase:` line: per phase that ran, `name=avg_ns/count`,
    /// in [`JniPhase`] order. `None` when off.
    pub fn line(&self) -> Option<String> {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.on {
            return None;
        }
        let mut out = String::from("[GC] jni_phase:");
        for (i, name) in JniPhase::NAMES.iter().enumerate() {
            let hits = self.hits[i].load(Relaxed);
            if hits == 0 {
                continue;
            }
            let avg = self.ns[i].load(Relaxed) / hits;
            out.push_str(&format!(" {name}={avg}ns/{hits}"));
        }
        Some(out)
    }
}

/// gce e1/j: the process VM's `[GC] jni_phase:` line, or `None` when its
/// census is off (or no VM is left). `vm-cli` prints it at exit.
pub fn jni_phase_census_line() -> Option<String> {
    process_vm()?.natives.jni_phase_census.line()
}

/// gce e1/j: the in-native deposit's sub-phase clock -- `None` unless the
/// census is on AND this deposit is the package's in-native one.
#[inline(always)]
pub(crate) fn deposit_phase_clock(census: &JniPhaseCensus) -> Option<std::time::Instant> {
    if census.on && crate::jit::conservative_roots::in_native_deposit_armed() {
        census.start()
    } else {
        None
    }
}

/// Running -> in native for `thread`'s JNI native call: the entry half of
/// `NativeContextImpl::begin_blocking_region`, minus its WAITING state and its
/// debugger inspection window (a thread in native is RUNNABLE, as in HotSpot).
///
/// gcd d10/j: minus also the blocked-stack summary of code reclamation
/// ([`crate::threading::gc_barrier::GcBarrier::mark_in_native_enter`]).
fn native_call_enter_native(shared: &SharedVm, thread: &mut JvmThread) {
    let census = &shared.natives.jni_phase_census;
    let mut clock = census.start();
    thread
        .gc_block_state
        .java_state
        .store(0, std::sync::atomic::Ordering::Release);
    // While still a counted mutator (a pause requested now waits for us), so
    // the tail filler cannot race its collector; see
    // `begin_blocking_region_with_state` for why a blocked thread must not
    // keep a TLAB.
    thread.tlab.retire();
    census.lap(JniPhase::TlabRetire, &mut clock);
    let tid = thread.thread_id;
    // gce e1/j (`gce-e1j-in-native-deposit-rescans-the-whole-stack-above-the-jit-chain`):
    // the deposit's unregistered-JIT-frame probes of the stack ABOVE the
    // outermost compiled entry -- the whole interpreter / Rust stack of the
    // thread, per call -- reuse a clean verdict while the JIT entry chain was
    // not touched since it was recorded; see "The frozen band of a JNI
    // in-native deposit" in `jit::conservative_roots`. Every other deposit
    // probes as before.
    crate::jit::conservative_roots::with_frozen_band_memo(Some(census), || {
        crate::vm::NativeContextImpl { shared, thread }.deposit_root_snapshot();
    });
    census.lap(JniPhase::Deposit, &mut clock);
    enter_in_native_region(shared, tid);
    census.stop(JniPhase::InNativeRegion, clock);
}

/// The barrier half of going into native, after the thread's
/// `in_blocked_region` went up: count it blocked, and arrive if a pause raced
/// in -- the flag went up before this check, so the pause in progress may
/// have excluded it already, and `auto` arrives only if it counted it.
fn enter_in_native_region(shared: &SharedVm, tid: ThreadId) {
    if shared.mem.gc_barrier.mark_in_native_enter() {
        let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
    }
}

/// gcd d10/j: running -> in native again after a function entered through
/// [`ForeignJniEntry::enter_vm_only`] that left native quietly, ran no Java,
/// and saw no pause complete and no fold since the call's last full deposit
/// (the caller checked all of that, with the record's `NativeSync`, its
/// `deposit_redefinitions` and its `appended` bound).
///
/// What the deposit on the way in published is still exact for everything
/// such a function cannot change: the interpreter frames (their slots,
/// `slot_origins` kept by the quiet leave, the class owners, the published
/// trace), the compiled frames and their pins, the stashed deopt frames, the
/// shadow stack, and the thread's off-frame fields (its ordinary path writes
/// no `JvmThread` field: it allocates through the heap, mints locals in the
/// thread-local frames and logs SATB entries; the one field a JNIEnv function
/// writes, `native_pending_return`, is set through `with_jni_context`, which
/// escalates the entry to the full deposit). What it can have added is the
/// locals it minted -- the entries of the local frames past `mark`, the
/// frames' extent when it left native ([`local_frames_mark`]) -- and those
/// are appended to the snapshot: appended, never replacing, so whatever the
/// snapshot held stays covered (a deleted local's referent is over-retained
/// until the next full deposit; no root is ever lost). The SATB entries it
/// logged are flushed and the TLAB retired, as the full deposit does. Then
/// the flag goes up and the thread is counted blocked again, exactly as on
/// the full path.
///
/// Returns how many entries it appended (the caller bounds the total).
fn native_call_reenter_incremental(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mark: LocalFramesMark,
) -> u32 {
    // As on the full path: no blocked thread keeps a TLAB.
    thread.tlab.retire();
    // `deposit_root_snapshot_inner`'s first step (fork6 GC_STRESS fix): a
    // store the function made during a concurrent mark must reach the
    // collector's queue before the thread is excluded from the remark.
    shared.mem.heap.flush_thread_satb();
    let appended = {
        let mut snapshot = thread.root_snapshot.lock();
        let before = snapshot.len();
        collect_local_ref_roots_since(mark, &mut snapshot);
        u32::try_from(snapshot.len() - before).unwrap_or(u32::MAX)
    };
    // After the snapshot is complete, as `deposit_root_snapshot_inner` raises
    // it: from here every pause folds this thread's moves into its fixup.
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, std::sync::atomic::Ordering::Release);
    enter_in_native_region(shared, thread.thread_id);
    appended
}

/// In native -> running: wait out a pause in progress, clear the flag under
/// the barrier lock, apply the fixups every collection of the window
/// accumulated (frames, native pins, JNI local frames, compiled frames) and
/// re-deposit -- `NativeContextImpl::end_blocking_region`.
///
/// gcd d5/f: `sync` is what [`NativeSync::read`] answered just before the
/// call went into native. When the barrier finds it unchanged at the moment
/// it clears the flag (`GcBarrier::leave_blocked_region_flagged_if`: under its
/// lock, no pause in progress or able to begin), no pause completed and no
/// relocation was folded into this thread for the whole window, so the full
/// wake has nothing to do: the fixup, the native-slot captures are empty, every
/// `slot_origins` entry still reads its origin, and the snapshot deposited on
/// the way in still describes the thread's frames exactly (a thread in native
/// changes none of them). That wake -- chiefly its snapshot refresh, a second
/// full deposit -- is skipped; only a class redefinition in the window still
/// needs the frames converted. Anything else takes the full wake as before.
///
/// gcd d10/j: the counter release and the flag clear are ONE barrier-lock
/// hold when no pause is in progress (`GcBarrier::leave_in_native_if`), and
/// no blocked-stack summary is withdrawn (none was published,
/// `mark_in_native_enter`). `keep_origins` (a function entered through
/// [`ForeignJniEntry::enter_vm_only`]) keeps `slot_origins` on a quiet leave
/// for the incremental re-entry. Returns whether the leave was quiet.
fn native_call_leave_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sync: NativeSync,
    keep_origins: bool,
) -> bool {
    let census = &shared.natives.jni_phase_census;
    let mut clock = census.start();
    let quiet = shared.mem.gc_barrier.leave_in_native_if(
        thread.thread_id,
        &thread.gc_block_state.in_blocked_region,
        || NativeSync::read(shared, &thread.gc_block_state) == sync,
    );
    census.lap(JniPhase::LeaveBarrier, &mut clock);
    if quiet {
        // Every entry still reads `cur == orig`; the next flag-raising
        // deposit records a fresh set. Cleared rather than left, so no later
        // wake can apply them to other frames -- unless the caller re-enters
        // on this very deposit (`keep_origins`), and then it clears them
        // itself on every other way out.
        if !keep_origins {
            thread.gc_block_state.slot_origins.lock().clear();
        }
        crate::runtime::interpreter::obsolete_frames::convert_obsolete_frames_if_redefined(
            shared, thread,
        );
        census.stop(JniPhase::LeaveQuietTail, clock);
        return true;
    }
    // The flag is still up: the full wake clears it itself.
    crate::vm::NativeContextImpl { shared, thread }.check_post_block_gc();
    census.stop(JniPhase::LeaveFullWake, clock);
    false
}

/// The in-native bracket of one JNI native call ([`dispatch_jni_native`]);
/// see the section comment above. Inert unless the flag is on and the
/// call qualifies.
#[must_use = "the call leaves native when the guard drops"]
pub(crate) struct JniNativeCall {
    /// This guard installed a record and must remove it (leaving native if
    /// the record is still in native).
    engaged: bool,
    /// The enclosing call's record, put back on drop.
    prev: Option<NativeCallRecord>,
}

impl JniNativeCall {
    /// Take the calling thread into native for its JNI native call.
    /// `escapes_before` is [`raw_local_escapes`] read before the receiver and
    /// the arguments were recorded: a raw one among them keeps the call
    /// counted.
    #[inline]
    pub(crate) fn enter(escapes_before: u64) -> Self {
        if !native_transitions_active() {
            return JniNativeCall {
                engaged: false,
                prev: None,
            };
        }
        Self::enter_engaged(escapes_before).unwrap_or(JniNativeCall {
            engaged: false,
            prev: None,
        })
    }

    #[inline(never)]
    fn enter_engaged(escapes_before: u64) -> Option<Self> {
        if !locals_are_indirect_here() || raw_local_escapes() != escapes_before {
            return None;
        }
        let shared = JNI_SHARED_VM
            .try_with(|c| c.try_borrow().ok().and_then(|b| b.clone()))
            .ok()
            .flatten()?;
        let thread = JNI_THREAD
            .try_with(|c| c.get())
            .unwrap_or(std::ptr::null_mut()) as *mut JvmThread;
        if thread.is_null() {
            return None;
        }
        // SAFETY: `JNI_THREAD` is the dispatching thread's live `JvmThread`,
        // installed by the dispatch's `JniContextGuard` for the whole call;
        // only an atomic is read here.
        let blocked = unsafe { &*thread }
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        if blocked {
            // Not a counted mutator to begin with (never expected here).
            return None;
        }
        // gcd d5/f: see `NativeCallRecord::block_state` / `::sync`. Read
        // before the deposit, so a pause completing during it counts.
        // SAFETY: as above; the `Arc` field is read, not the thread mutated.
        let block_state = Arc::as_ptr(&unsafe { &*thread }.gc_block_state);
        // SAFETY: `block_state` points into the live `JvmThread` (above).
        let sync = NativeSync::read(&shared, unsafe { &*block_state });
        // SAFETY: as above; a `Copy` field of the live `JvmThread`.
        let deposit_redefinitions = unsafe { (*thread).redefinitions_seen };
        // gce e1/j: the one `Arc` taken above moves into the record (it was
        // cloned a second time per call); the transition below borrows the VM
        // through this pointer, which the record -- and the dispatch's own
        // `JNI_SHARED_VM` context -- keep alive until this guard drops.
        let shared_ptr = Arc::as_ptr(&shared);
        let prev = JNI_NATIVE_CALL
            .try_with(|t| {
                t.record.try_borrow_mut().ok().map(|mut slot| {
                    slot.replace(NativeCallRecord {
                        shared,
                        thread,
                        in_native: true,
                        raw_escapes: escapes_before,
                        block_state,
                        sync,
                        deposit_redefinitions,
                        appended: 0,
                    })
                })
            })
            .ok()
            .flatten()?;
        // SAFETY: as above; the `&mut` lives only for the transition, while
        // the interpreter that holds the thread is suspended in this call.
        // `shared_ptr`: the record just installed owns the `Arc`, and only
        // this guard's drop removes it.
        native_call_enter_native(unsafe { &*shared_ptr }, unsafe { &mut *thread });
        Some(JniNativeCall {
            engaged: true,
            prev,
        })
    }
}

impl Drop for JniNativeCall {
    fn drop(&mut self) {
        if !self.engaged {
            return;
        }
        let prev = self.prev.take();
        let mine = JNI_NATIVE_CALL
            .try_with(|t| {
                t.record
                    .try_borrow_mut()
                    .ok()
                    .and_then(|mut slot| std::mem::replace(&mut *slot, prev))
            })
            .ok()
            .flatten();
        if let Some(rec) = mine {
            if rec.in_native {
                // SAFETY: see `enter_engaged`.
                let _ = native_call_leave_native(
                    &rec.shared,
                    unsafe { &mut *rec.thread },
                    rec.sync,
                    false,
                );
            }
        }
    }
}

/// Take (read + clear) the pending JNI exception, if any.
///
/// Returns `Some(raw_value)` if an exception was set via `Throw` or `ThrowNew`,
/// and clears it so subsequent calls return `None`. The raw value is the
/// JNI-level handle (u64); `u64::MAX` is only a fallback marker when a JNI
/// helper could not materialise a Java exception because no VM thread context
/// was available.
pub fn take_jni_pending_exception() -> Option<u64> {
    JNI_PENDING_EXCEPTION.with(|cell| {
        let val = cell.get();
        if val != 0 {
            cell.set(0);
            Some(val)
        } else {
            None
        }
    })
}

/// Publish a real pending JNI exception and keep it in the VM's GC-updated
/// native-return root slot until the interpreter observes it.  The raw JNI
/// handle remains available to `ExceptionOccurred`, while `native_pending_return`
/// is the authoritative (and moving-GC-safe) reference at native return.
fn set_jni_pending_exception_object(exception: ObjectRef) {
    let handle = obj_to_jobject(exception);
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(handle));
    let _ = with_jni_context(|_shared, thread| {
        thread.native_pending_return = Some(exception);
    });
}

/// Access the VM context from within a JNI function. Returns None if not set.
///
/// The `Arc<SharedVm>` in TLS guarantees the VM is alive for the entire
/// duration of the closure — no raw-pointer cast, no use-after-free risk.
fn with_shared_vm<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SharedVm) -> R,
{
    JNI_SHARED_VM.with(|c| {
        let borrow = c.borrow();
        match borrow.as_ref() {
            None => None,
            Some(arc) => {
                let gen_before = JNI_CONTEXT_GENERATION.with(|g| g.get());
                let result = f(arc);
                let gen_after = JNI_CONTEXT_GENERATION.with(|g| g.get());
                if gen_before != gen_after {
                    // Nesting is legal and handled: `JniContextGuard` saves and
                    // restores rather than clearing, so a nested native call
                    // leaves this thread's context exactly as it found it. The
                    // generation still moves (twice), which is why this is a
                    // debug note and not a warning — it fires on every correct
                    // native -> Java -> native chain.
                    tracing::debug!(
                        "JNI context generation changed during callback ({} -> {}) — a nested native call, which restore-on-exit handles",
                        gen_before,
                        gen_after
                    );
                }
                Some(result)
            }
        }
    })
}

/// Access both SharedVm and JvmThread from within a JNI function.
/// The JvmThread borrow is exclusive for the duration of the closure.
/// Returns None if either context is not set.
fn with_jni_context<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SharedVm, &mut JvmThread) -> R,
{
    // gcd d5/f: a leaf JNIEnv function that got here (to raise) leaves its
    // leaf window for the full transition first. One thread-local read.
    escalate_leaf_window();
    let shared_arc: Option<Arc<SharedVm>> = JNI_SHARED_VM.with(|c| c.borrow().clone());
    let thread_ptr = JNI_THREAD.with(|c| c.get());
    match (shared_arc, thread_ptr.is_null()) {
        (Some(ref arc), false) => {
            // Safety: JNI_THREAD is set to a valid JvmThread pointer by set_jni_thread
            // and cleared by clear_jni_thread. The pointer is only used from the owning
            // thread (thread-local), and we hold exclusive access for the closure duration.
            Some(f(arc, unsafe { &mut *(thread_ptr as *mut JvmThread) }))
        }
        _ => {
            // A `None` here is answered to the host library as a null `jobject`
            // or a zero, which it reads as an ordinary answer. That silence is
            // what made the TLSv1.3 client-certificate defect cost a week: a
            // nested native call used to CLEAR the enclosing call's context, and
            // every later up-call the outer native made landed here and was
            // quietly ignored. See `JniContextGuard::install`.
            //
            // A `warn!` is wrong — a genuinely detached host thread reaches this
            // legitimately and often — so the instrument is a count, reported
            // with the other JNI figures. Zero is the expected value for any run
            // whose native calls all originate from Java.
            JNI_UPCALLS_WITHOUT_CONTEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }
}

/// The registry id of the `JvmThread` this OS thread's JNI binding names, if
/// one is installed.
///
/// Reads one `Copy` field through the `JNI_THREAD` pointer without forming a
/// `&mut` (so it cannot alias an interpreter borrow the way
/// [`with_jni_context`] can), and without counting a miss in
/// [`JNI_UPCALLS_WITHOUT_CONTEXT`]. `try_with`: callable from a thread whose
/// TLS is being torn down (the answer is then "unbound").
fn jni_bound_thread_id() -> Option<ThreadId> {
    let p = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null `JNI_THREAD` points at a live `JvmThread` for as long
    // as it stays installed (`set_jni_thread`'s contract); `thread_id` is an
    // immutable `Copy` field written once at construction.
    Some(unsafe { (*(p as *const JvmThread)).thread_id })
}

/// The CALLER's registry id, for anything keyed on thread identity — monitor
/// ownership and the STW barrier (gc-common w2-c, 2026-09-23;
/// `common-a-jni-placeholder-thread-id-at-barrier`).
///
/// JNI binding first (it is exact), then the published OS tid, which for a
/// carrier answers the MOUNTED virtual thread (the newest publish on this OS
/// thread, gc-common w6-g). `None` means unregistered (or, for entries not
/// published through `set_os_tid_current`, still ambiguous): callers must NOT
/// substitute a placeholder
/// id at the barrier. `ThreadId(0)` is the MAIN thread's real id, so arriving
/// under it either makes the caller look like the initiator (a counted caller
/// never arrives: hang) or fills main's quota slot (early release while main
/// still runs: a moving collection over live frames).
fn jni_caller_thread_id(shared: &SharedVm) -> Option<ThreadId> {
    jni_bound_thread_id().or_else(|| {
        shared
            .threads
            .thread_registry
            .thread_id_for_current_os_tid()
    })
}

/// How many JNI up-calls found no thread context and were answered as if the VM
/// were detached.
///
/// Reported by `CRATONVM_INTRINSIC_STATS=1`. See the `_` arm of
/// [`with_jni_context`] for what a non-zero count means and why it is a counter
/// rather than a warning.
pub static JNI_UPCALLS_WITHOUT_CONTEXT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of JNI up-calls answered with no context since process start.
pub fn jni_upcalls_without_context() -> u64 {
    JNI_UPCALLS_WITHOUT_CONTEXT.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// JValue union (mirrors C `jvalue` union from jni.h)
// ---------------------------------------------------------------------------

/// C-compatible union of all JNI primitive and reference types.
/// Used by the CallXxxMethodA / CallStaticXxxMethodA families.
#[repr(C)]
pub union JValue {
    pub z: JBoolean, // boolean
    pub b: JByte,    // byte
    pub c: JChar,    // char
    pub s: JShort,   // short
    pub i: JInt,     // int
    pub j: JLong,    // long
    pub f: JFloat,   // float
    pub d: JDouble,  // double
    pub l: JObject,  // object / array reference
}

// ---------------------------------------------------------------------------
// Descriptor parsing helpers
// ---------------------------------------------------------------------------

/// Look up parsed parameter types from the thread-local cache, or parse and cache.
fn parse_param_types_cached(descriptor: &str) -> Vec<u8> {
    JNI_DESCRIPTOR_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.get(descriptor) {
            return cached.to_vec();
        }
        let types = parse_param_types_inner(descriptor);
        // Bounded LRU: evicts a single least-recently-used entry when full,
        // avoiding the periodic full-flush thrash of a wholesale clear().
        cache.insert(descriptor, types.clone());
        types
    })
}

/// gcd d10/j (`gcd-d5f-jni-default-dispatch-costs-500ns`): the tags of
/// [`parse_param_types_inner`], produced in place -- no thread-local cache
/// probe (a `String`-keyed hash of the descriptor), no `Vec` copy per call.
/// [`dispatch_jni_native`]'s hot path reads them once each. Same bytes, same
/// order, same answer for a malformed descriptor
/// (`param_type_tags_match_the_parser`).
struct ParamTypeTags<'a> {
    bytes: &'a [u8],
    i: usize,
}

/// See [`ParamTypeTags`].
fn param_type_tags(descriptor: &str) -> ParamTypeTags<'_> {
    ParamTypeTags {
        bytes: descriptor.as_bytes(),
        i: 1, // skip leading '('
    }
}

impl ParamTypeTags<'_> {
    /// Skip past the next `;` (an object type's name and its terminator).
    #[inline]
    fn skip_class_name(&mut self) {
        while self.i < self.bytes.len() && self.bytes[self.i] != b';' {
            self.i += 1;
        }
        self.i += 1; // skip ';'
    }
}

impl Iterator for ParamTypeTags<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        // Exactly `parse_param_types_inner`'s loop, one tag per call; an
        // unknown byte is skipped as it skips it.
        while self.i < self.bytes.len() && self.bytes[self.i] != b')' {
            match self.bytes[self.i] {
                b @ (b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D') => {
                    self.i += 1;
                    return Some(b);
                }
                b'L' => {
                    self.i += 1;
                    self.skip_class_name();
                    return Some(b'L');
                }
                b'[' => {
                    self.i += 1;
                    if self.i < self.bytes.len() && self.bytes[self.i] == b'L' {
                        self.i += 1;
                        self.skip_class_name();
                    } else if self.i < self.bytes.len() && self.bytes[self.i] == b'[' {
                        while self.i < self.bytes.len() && self.bytes[self.i] == b'[' {
                            self.i += 1;
                        }
                        if self.i < self.bytes.len() && self.bytes[self.i] == b'L' {
                            self.i += 1;
                            self.skip_class_name();
                        } else {
                            self.i += 1;
                        }
                    } else {
                        self.i += 1; // primitive element type
                    }
                    return Some(b'[');
                }
                _ => {
                    self.i += 1;
                }
            }
        }
        None
    }
}

/// Parse the parameter type tags from a JNI method descriptor, e.g.
/// `(ILjava/lang/String;[BZ)V` → `[b'I', b'L', b'[', b'Z']`.
/// Object types (`L...;`) and array types (`[...`) are both returned as a
/// single token (`b'L'` and `b'['` respectively).
fn parse_param_types_inner(descriptor: &str) -> Vec<u8> {
    let mut types = Vec::new();
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip leading '('
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {
                types.push(bytes[i]);
                i += 1;
            }
            b'L' => {
                types.push(b'L');
                i += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // skip ';'
            }
            b'[' => {
                types.push(b'[');
                i += 1;
                // Skip the element type (may itself be an object or array)
                if i < bytes.len() && bytes[i] == b'L' {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1; // skip ';'
                } else if i < bytes.len() && bytes[i] == b'[' {
                    // multi-dimensional: skip extra '[' characters
                    while i < bytes.len() && bytes[i] == b'[' {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == b'L' {
                        i += 1;
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1; // primitive element type
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    types
}

/// Long-smuggle mint chokepoint for the JNI paths that turn a native `jlong`
/// into a Java `long` without going through `SetLongField` /
/// `SetLongArrayRegion` / the generic-JNI `J` return (which mint inline):
/// call arguments (`jvalues_to_values`, which the `va_list` forms reach via
/// `va_list_to_jvalues`), `SetStaticLongField`, and the long-array write-back
/// of `Release<Long>ArrayElements` / `ReleasePrimitiveArrayCritical`. Same
/// strict object-start probe as those sites, so ordinary numeric values
/// register nothing (see `memory::smuggled_longs`).
#[inline]
fn mint_if_smuggled_jlong(heap: &crate::memory::vm_heap::VmHeap, bits: u64) {
    if bits != 0 && bits & 0x7 == 0 && heap.is_object_address(bits as usize).is_some() {
        crate::memory::smuggled_longs::record_minted_long(heap, bits);
    }
}

/// [`mint_if_smuggled_jlong`] for an already-decoded element value: a no-op
/// for everything but `Value::Long`.
#[inline]
fn mint_if_smuggled_long_value(heap: &crate::memory::vm_heap::VmHeap, value: &Value) {
    if let Value::Long(l) = *value {
        mint_if_smuggled_jlong(heap, l as u64);
    }
}

/// Convert a slice of `JValue`s to `Value`s using the type tags from
/// `parse_param_types`. Returns an empty vector if `args` is null.
///
/// A `J` argument is a mint site (`mint_if_smuggled_jlong`):
/// `CallVoidMethod(obj, setHandle, (jlong) someJobject)` is the classic
/// long-as-jobject smuggle, and every `Call*Method{,V,A}` / `NewObject{,V,A}`
/// funnels through here.
unsafe fn jvalues_to_values(
    heap: &crate::memory::vm_heap::VmHeap,
    args: *const JValue,
    types: &[u8],
) -> Vec<Value> {
    if args.is_null() {
        return Vec::new();
    }
    types
        .iter()
        .enumerate()
        .map(|(idx, &tag)| {
            let jv = &*args.add(idx);
            match tag {
                b'Z' => Value::Int(jv.z as i32),
                b'B' => Value::Int(jv.b as i32),
                b'C' => Value::Int(jv.c as i32),
                b'S' => Value::Int(jv.s as i32),
                b'I' => Value::Int(jv.i),
                b'J' => {
                    mint_if_smuggled_jlong(heap, jv.j as u64);
                    Value::Long(jv.j)
                }
                b'F' => Value::Float(jv.f),
                b'D' => Value::Double(jv.d),
                _ /* b'L' | b'[' */ => match jobject_to_obj(jv.l) {
                    Some(r) => Value::Object(Some(r)),
                    None => Value::Object(None),
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Core call helpers used by all Call*MethodA variants
// ---------------------------------------------------------------------------
//
// JDK-only mode (`docs/book/src/user-guide/jdk-only-mode.md` §7): JNI does **not**
// carry a second native-vs-bytecode resolver. Every `Call*Method*` family
// member below funnels into `invoke_on_class_shared`, which is routed through
// `resolve_dispatch`, so JNI inherits the single policy-aware decision point
// (and the §4 invocation census taken there) for free. That is exactly the
// duplicate-dispatch bypass this wave exists to close: the fix here is *not* to
// add a registry lookup of our own.
//
// What JNI does have to add is the **surfacing** rule. These helpers return
// `Option<Value>` and drop `Err` on the floor (`.ok().flatten()`), so a
// `JdkOnlyViolation` raised behind a `CallObjectMethod` would vanish and the run
// would report zero violations having just taken the forbidden path — an
// unverifiable path, which §11's "zero synthetic-stub invocations through any
// path" cannot tolerate. [`jni_surface_jdk_only`] intercepts that one case.

/// Convert an `invoke_on_class_shared` result into JNI's `Option<Value>`,
/// surfacing a JDK-only refusal as a pending JNI exception instead of `None`.
///
/// **`Compatible` mode is bit-for-bit unchanged**: the only intercepted value is
/// `VmError::JdkOnly`, which `Compatible` mode never constructs. Every other
/// outcome — including a Java `ExceptionThrown`, which these JNI helpers have
/// always dropped (a separate, pre-existing gap; see the JDK-ONLY-NOTE below) —
/// takes precisely the path it took before.
///
/// JDK-ONLY-NOTE — **CLOSED.** This used to read "the JNI `Call*Method` helpers
/// silently swallow `MethodCallFailed::ExceptionThrown`, so a Java exception
/// thrown by a JNI-initiated call never becomes a pending JNI exception. Fixing
/// it would alter `Compatible` behaviour, which this wave may not do." The
/// `ExceptionThrown` arm below has published the pending exception since the
/// bare-varargs `Call<T>Method` slots started dispatching, so the note has been
/// contradicted by the code thirty lines under it. Corrected 2026-08-06.
///
/// What is still dropped is every OTHER `Err(_)`, which is the honest residual:
/// an internal VM failure has no JNI representation short of inventing one.
/// Except under `--jdk-only` (round 12 wave 6, lane jni): a Java-typed VM
/// error (`VmError::Runtime` / `VmError::Linkage`) is the throwable it names
/// ([`jni_vm_error_as_throwable`]) and is published pending like any other.
/// That is the shape a registered native's failure takes when the up-call
/// target IS a native (`Class.forName0` answering `ClassNotFoundException`),
/// which an interpreter invoke arm materialises and this used to drop, leaving
/// the native a 0 with nothing pending.
#[inline]
fn jni_surface_jdk_only(
    shared: &SharedVm,
    thread: &mut JvmThread,
    result: crate::error::MethodCallResult,
) -> Option<Value> {
    let result = match result {
        Err(err) if jni_types_vm_errors(shared) => {
            Err(jni_vm_error_as_throwable(shared, thread, err))
        }
        other => other,
    };
    match result {
        Ok(value) => value,
        Err(crate::error::MethodCallFailed::InternalError(crate::error::VmError::JdkOnly(
            violation,
        ))) => {
            raise_jdk_only_violation(shared, thread, &violation);
            None
        }
        // An ordinary Java exception thrown by the method the native called.
        //
        // JNI's contract is that it is PENDING when the up-call returns, so
        // the native's `ExceptionCheck` sees it and can decide what to do;
        // discarding it — which this arm used to do, along with every other
        // failure — meant a native saw a `0` return from a method that had in
        // fact thrown, with nothing pending to say so. That was unobservable
        // for as long as the bare-varargs `Call<T>Method(...)` slots raised
        // UnsatisfiedLinkError instead of dispatching; it is reachable now
        // that they work.
        //
        // Published in place rather than through
        // `set_jni_pending_exception_object`, which would re-enter
        // `with_jni_context` for the `&mut JvmThread` this scope already
        // holds. Same two writes: the raw handle for `ExceptionOccurred`, and
        // the GC-remapped `native_pending_return` that is authoritative at the
        // native's own return.
        Err(crate::error::MethodCallFailed::ExceptionThrown(exc)) => {
            let handle = obj_to_jobject(exc);
            JNI_PENDING_EXCEPTION.with(|cell| cell.set(handle));
            thread.native_pending_return = Some(exc);
            None
        }
        Err(_) => None,
    }
}

/// Does this VM publish a Java-typed VM error an up-call returned as the
/// throwable it names ([`jni_surface_jdk_only`])? In both modes: the owner
/// turned it on for `--compatible` too (2026-09-27). Kill switch
/// `CRATONVM_JNI_UPCALL_TYPED_ERRORS=0`.
fn jni_types_vm_errors(_shared: &SharedVm) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JNI_UPCALL_TYPED_ERRORS")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("0" | "false" | "off" | "no")
        )
    })
}

/// `VmError::Runtime` / `VmError::Linkage` as the Java throwable each names,
/// thrown (`MethodCallFailed::ExceptionThrown`), the conversion the
/// interpreter's invoke arms make (`classify_fastpath_invoke_error`); any
/// other failure unchanged. Cold: error paths only.
#[cold]
fn jni_vm_error_as_throwable(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
) -> crate::error::MethodCallFailed {
    match err {
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(re)) => {
            crate::runtime::exceptions::throw_runtime_error(shared, thread, re)
        }
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Linkage(le)) => {
            crate::runtime::exceptions::throw_linkage_error(shared, thread, le)
        }
        other => other,
    }
}

/// Materialise a JDK-only refusal as a pending JNI exception.
///
/// Mirrors [`jni_throw_unsatisfied_link`]'s shape, but takes the already-held
/// `shared`/`thread` rather than re-entering `with_jni_context` — these call
/// sites are *inside* that closure. Cold: only ever reached on a refusal, so the
/// `String` the message needs is never built on the fast path.
///
/// The exception class is `java/lang/InternalError`, and as of 2026-08-06 that
/// is a decision rather than a placeholder. The open question this comment used
/// to carry — "if `--explain-jdk-only`'s rendering settles on a different
/// Java-visible type, follow it" — was checked and has no answer to follow:
/// `JdkOnlyViolation::render` produces a text/JSON report and names no Java
/// type, so nothing else in the tree materialises a violation as a throwable.
///
/// `InternalError` satisfies the constraint that actually matters, which is
/// that a policy refusal must be **catchable**: this builds a real exception
/// object and leaves it pending, so a native's `ExceptionCheck` sees it and a
/// `catch (Throwable)` can observe it. That is the opposite of the bare
/// `MethodCallFailed::InternalError` the other refusal sites return, whose own
/// doc says it is "not catchable by Java code" and aborts the run — this JNI
/// path is the one place a `--jdk-only` refusal is already the right shape.
/// Contract §5's spec-appropriate types (`NoClassDefFoundError` and friends,
/// via `native_api::refusal_to_java_failure`) apply to class-identity
/// refusals; a dispatch-policy refusal is not one of those, so it does not
/// inherit their type. Revisit only if a Java-visible type is chosen elsewhere.
/// The message body is `JdkOnlyViolation::summary()` either way.
#[cold]
#[inline(never)]
fn raise_jdk_only_violation(
    shared: &SharedVm,
    thread: &mut JvmThread,
    violation: &cratonvm_types::error::JdkOnlyViolation,
) {
    let msg = violation.summary();
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/InternalError",
        Some(&msg),
    ) {
        Ok(exc) => set_jni_pending_exception_object(exc),
        // Heap exhausted / class-load failure while building the report: fall
        // back to the `ThrowNew` sentinel so the refusal is flagged rather than
        // swallowed, exactly as `jni_throw_unsatisfied_link` does. gcd d4/k:
        // a heap failure pends the preallocated OOME instead (a real object
        // for `ExceptionOccurred`; `pend_unbuilt_throwable`).
        Err(e) => {
            let oom = if is_heap_exhaustion(&e) {
                *shared.mem.singleton_oom.read()
            } else {
                None
            };
            match oom {
                Some(oom) => set_jni_pending_exception_object(oom),
                None => JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX)),
            }
        }
    }
}

/// Perform a virtual instance method call from JNI.
/// `obj` is the receiver; `mid` encodes (declaring_class_id, method_index);
/// `args` is the JValue array (may be null for zero-arg methods).
fn jni_call_instance(obj: JObject, mid: JMethodID, args: *const JValue) -> Option<Value> {
    if obj == 0 || mid == 0 {
        return None;
    }
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj)?;
        let (decl_class_id, method_index) = decode_method_id(mid);
        // An array's header carries its COMPONENT's class id, so dispatching
        // on it ran the component's override with the array as `this`
        // (`CallObjectMethod(stringArray, toString)` reached
        // `String.toString`). An array inherits every instance method from
        // `java/lang/Object` and overrides none, so its virtual target is the
        // method ID's own declaring class; `invoke_on_class_shared` does not
        // retarget an array receiver.
        let is_array = shared.mem.heap.kind_of(oref) == crate::memory::heap::ObjectKind::Array;
        let obj_class_id = if is_array {
            decl_class_id
        } else {
            shared.mem.heap.class_id_of(oref)
        };
        let (method_name, descriptor) = {
            let cm = shared.classes.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(&shared.mem.heap, args, &param_types) });
        // JDK-only §7: the resolver runs inside `invoke_on_class_shared`; this
        // site only has to keep its refusal from being swallowed.
        let result = invoke_on_class_shared(
            shared,
            thread,
            obj_class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        );
        jni_surface_jdk_only(shared, thread, result)
    })
    .flatten()
}

/// Perform a nonvirtual instance method call from JNI (dispatch on `clazz`,
/// not on the runtime type of `obj`).
fn jni_call_nonvirtual(
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> Option<Value> {
    if obj == 0 || mid == 0 {
        return None;
    }
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj)?;
        let dispatch_class_id = if clazz != 0 {
            jclass_class_id(clazz)
        } else {
            let (dcid, _) = decode_method_id(mid);
            dcid
        };
        let (_, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.classes.class_manager.read();
            let class = cm.class_store.get(dispatch_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(&shared.mem.heap, args, &param_types) });
        // JDK-only §7: resolver runs inside `invoke_on_class_shared`.
        let result = invoke_on_class_shared(
            shared,
            thread,
            dispatch_class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        );
        jni_surface_jdk_only(shared, thread, result)
    })
    .flatten()
}

/// Perform a static method call from JNI.
fn jni_call_static(clazz: JClass, mid: JMethodID, args: *const JValue) -> Option<Value> {
    if mid == 0 {
        return None;
    }
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let (decl_class_id, method_index) = decode_method_id(mid);
        let class_id = if clazz != 0 {
            jclass_class_id(clazz)
        } else {
            decl_class_id
        };
        let (method_name, descriptor) = {
            let cm = shared.classes.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let jvm_args = unsafe { jvalues_to_values(&shared.mem.heap, args, &param_types) };
        // JDK-only §7: resolver runs inside `invoke_on_class_shared`.
        let result = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        );
        jni_surface_jdk_only(shared, thread, result)
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// Global Reference Table
// ---------------------------------------------------------------------------
//
// Design: each global ref is stored in a heap-allocated `Box<ObjectRef>`.
// The `JObject` handle returned to native code has bit 0 set (tag bit) to
// distinguish global refs from ordinary local refs (raw heap pointers, which
// are at least 8-byte aligned so their bit 0 is always 0).
//
// Global ref handle encoding:
//   `handle = (Box::into_raw(box_ptr) as usize) | 1`
//
// Decoding (in `jobject_to_obj`):
//   if `handle & 1 == 1` → `*(handle & !1) as *const ObjectRef`
//   else                 → `handle` is a raw heap pointer
//
// WEAK global refs (gc-common w8-c, `common-w7c-jni-weak-global-refs-are-strong`)
// use the same encoding, but their box is a `Box<usize>` holding the referent's
// address, or 0 once the referent died, and it is kept in a SEPARATE set:
//
// * `collect_roots` and `update_after_gc` never see a weak entry, so a weak
//   global is not a GC root and is not remapped by `update_all_roots`;
// * [`sweep_weak_global_refs`] (every stop-the-world collection) remaps a weak
//   entry whose referent survived and clears one whose referent died;
//   [`clear_weak_global_refs_unmarked`] clears at a concurrent cycle's remark
//   (the bitmap verdict, before anything is freed), and the out-of-pause
//   address-keyed sweeps clear what a concurrent reclamation freed;
// * resolving a live weak handle is a keep-alive (the SATB enqueue
//   `Reference.get()` uses), so a referent a native picks up during a
//   concurrent mark cannot be freed by that cycle.
//
// A cleared entry stays until `DeleteWeakGlobalRef`, as the JNI spec requires:
// the handle is still valid, it just reads as NULL.

pub struct JniGlobalRefs {
    /// Raw `Box<ObjectRef>` pointers (stored as usize for Send/Sync).
    /// Each entry owns its allocation until `remove` is called. Keyed by the
    /// pointer itself so `resolve`/`remove` are O(1) on this hot path (the
    /// handle is just `raw | 1`, so the untagged pointer is a unique key —
    /// distinct `Box` allocations never collide).
    entries: HashSet<usize>,
    /// gc-common w8-c: raw `Box<usize>` pointers of the WEAK global refs. The
    /// box holds the referent's current address, or 0 once it was cleared.
    /// Disjoint from `entries` (distinct live `Box` allocations never share
    /// an address), not rooted, and not remapped by `update_after_gc`: see
    /// [`sweep_weak_global_refs`].
    weak: HashSet<usize>,
    /// gc-common w10-c (`common-w9g-jni-element-copies-are-bound-to-the-getting-thread`):
    /// the `Get<Type>ArrayElements` copies outstanding in THIS VM, keyed by
    /// the buffer address handed to native code. Each record's `array_gref`
    /// is a strong entry of this table. They used to live in a
    /// `thread_local!`, so a `Release` on another thread (legal: the JNI spec
    /// binds a `JNIEnv`, not an elements pointer, to its thread) missed the
    /// record, lost the write-back, leaked the buffer and never deleted the
    /// keep-alive global ref: the array was immortal. Keyed by C-heap
    /// addresses, not `ObjectRef`s, so no collection touches the keys.
    elem_copies: HashMap<usize, ArrayElemBuffer>,
    /// The `GetStringChars` / `GetStringCritical` buffers outstanding in this
    /// VM: buffer address -> UTF-16 unit count, the layout `ReleaseStringChars`
    /// rebuilds to free it. Per VM for the same reason (gc-common w10-c).
    string_copies: HashMap<usize, usize>,
}

/// What kind of entry a tagged global handle names (gc-common w8-c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JniGlobalKind {
    /// `NewGlobalRef` (and the VM's own users of the table).
    Strong,
    /// `NewWeakGlobalRef`.
    Weak,
}

/// What one weak-global sweep did, for tests and diagnostics (gc-common w8-c).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WeakGlobalSweepStats {
    /// Referents the collection relocated; the entry now names the new address.
    pub moved: usize,
    /// Referents that survived in place.
    pub retained: usize,
    /// Referents that died this cycle; the entry now reads as NULL.
    pub cleared: usize,
}

// Safety: `JniGlobalRefs` is stored behind `parking_lot::Mutex<>` in `SharedVm`.
// All mutations (`add`, `remove`, `update_after_gc`) require `&mut self`, enforced by
// the Mutex.  All reads (`resolve`, `collect_roots`, `count`) also go through the
// Mutex lock.  Critically, `jobject_to_obj()` acquires the same Mutex when resolving
// global ref handles, preventing a use-after-free race between concurrent `resolve()`
// and `remove()` calls.
unsafe impl Send for JniGlobalRefs {}
unsafe impl Sync for JniGlobalRefs {}

impl JniGlobalRefs {
    pub fn new() -> Self {
        Self {
            entries: HashSet::new(),
            weak: HashSet::new(),
            elem_copies: HashMap::new(),
            string_copies: HashMap::new(),
        }
    }

    /// Record a `Get<Type>ArrayElements` copy of `array` at `ptr` (`len`
    /// initialised elements of a `cap`-element `Vec`), minting the strong
    /// global ref that keeps the array alive and follows it through every
    /// relocation until the final release. gc-common w10-c.
    fn open_elem_copy(&mut self, ptr: usize, len: usize, cap: usize, array: ObjectRef) {
        let array_gref = self.add(array);
        if let Some(stale) = self.elem_copies.insert(
            ptr,
            ArrayElemBuffer {
                len,
                cap,
                array_gref,
            },
        ) {
            // Unreachable: a live buffer's address is unique, and a released
            // one's record went with its release. Never leave its root behind.
            self.remove(stale.array_gref);
        }
    }

    /// The copy record at `ptr`, taken out of the table (`take`, a freeing
    /// release) or left in place (`JNI_COMMIT`). The caller deletes the
    /// global ref once it has copied back.
    fn elem_copy(&mut self, ptr: usize, take: bool) -> Option<ArrayElemBuffer> {
        if take {
            self.elem_copies.remove(&ptr)
        } else {
            self.elem_copies.get(&ptr).copied()
        }
    }

    /// Record a `GetStringChars` buffer of `len` UTF-16 units at `ptr`.
    fn open_string_copy(&mut self, ptr: usize, len: usize) {
        self.string_copies.insert(ptr, len);
    }

    /// Take the `GetStringChars` record at `ptr` out of the table.
    fn take_string_copy(&mut self, ptr: usize) -> Option<usize> {
        self.string_copies.remove(&ptr)
    }

    /// How many element and string copies native code has not released yet in
    /// this VM. Nonzero at the end of a run means a native leaked a `Get` (for
    /// an element copy that is an array kept alive by its global ref). Not
    /// `pub`: only the tests read it (`no_test_only_public_api`).
    fn outstanding_copies(&self) -> usize {
        self.elem_copies.len() + self.string_copies.len()
    }

    /// Create a WEAK global ref for `obj` (gc-common w8-c). Returns a tagged
    /// `JObject` handle, encoded like a strong one. See the section comment
    /// above for what makes it weak.
    pub fn add_weak(&mut self, obj: ObjectRef) -> JObject {
        let boxed: Box<usize> = Box::new(obj.as_ptr() as usize);
        let raw = Box::into_raw(boxed) as usize; // OWNERSHIP: transferred to self.weak, freed by JniGlobalRefs::remove() or Drop impl
        self.weak.insert(raw);
        (raw | 1) as JObject
    }

    /// Is `handle` a live WEAK entry? Cheap when no weak global exists (the
    /// strong-only hot path pays one `is_empty`).
    pub fn is_weak(&self, handle: JObject) -> bool {
        handle & 1 == 1
            && !self.weak.is_empty()
            && self.weak.contains(&((handle & !1) as usize))
    }

    /// Strong, weak, or not a handle of this table (`None`: 0, an untagged
    /// handle, or one already deleted).
    pub fn kind(&self, handle: JObject) -> Option<JniGlobalKind> {
        if handle == 0 || handle & 1 == 0 {
            return None;
        }
        let raw = (handle & !1) as usize;
        if self.entries.contains(&raw) {
            Some(JniGlobalKind::Strong)
        } else if self.weak.contains(&raw) {
            Some(JniGlobalKind::Weak)
        } else {
            None
        }
    }

    /// How many weak global refs exist, cleared ones included (a cleared
    /// entry lives until `DeleteWeakGlobalRef`). Test-only: nothing in the VM
    /// needs the figure (`no_test_only_public_api`).
    #[cfg(test)]
    fn weak_count(&self) -> usize {
        self.weak.len()
    }

    /// The current referent address of every UNCLEARED weak entry, as
    /// `(box pointer, address)`. The sweeps snapshot this, judge the addresses
    /// with no table lock held, then apply the verdicts through
    /// [`Self::apply_weak_verdicts`].
    fn weak_snapshot(&self) -> Vec<(usize, usize)> {
        self.weak
            .iter()
            .filter_map(|&raw| {
                // Safety: raw is a valid Box<usize> pointer that we own.
                let addr = unsafe { *(raw as *const usize) };
                (addr != 0).then_some((raw, addr))
            })
            .collect()
    }

    /// Write each `(box pointer, expected address, new address)` verdict: the
    /// new address (0 = cleared) replaces the slot only if the entry still
    /// exists and still holds `expected`, so an entry deleted, or rewritten by
    /// an interleaved sweep, since the snapshot is left alone.
    fn apply_weak_verdicts(&mut self, verdicts: &[(usize, usize, usize)]) {
        for &(raw, expected, new_addr) in verdicts {
            if !self.weak.contains(&raw) {
                continue;
            }
            let slot = raw as *mut usize;
            // Safety: raw is a valid Box<usize> pointer that we own (checked
            // just above, under `&mut self`).
            unsafe {
                if *slot == expected {
                    *slot = new_addr;
                }
            }
        }
    }

    /// Create a global ref for `obj`. Returns a tagged `JObject` handle.
    pub fn add(&mut self, obj: ObjectRef) -> JObject {
        let boxed: Box<ObjectRef> = Box::new(obj);
        let raw = Box::into_raw(boxed) as usize; // OWNERSHIP: transferred to self.entries, freed by JniGlobalRefs::remove() or Drop impl
        self.entries.insert(raw);
        (raw | 1) as JObject
    }

    /// Release a global ref created by `add`. Returns `true` if found and removed.
    pub fn remove(&mut self, handle: JObject) -> bool {
        if handle & 1 == 0 {
            return false; // not a global ref handle
        }
        let raw = (handle & !1) as usize;
        if self.entries.remove(&raw) {
            // Safety: raw was created by Box::into_raw and we own it.
            unsafe { drop(Box::from_raw(raw as *mut ObjectRef)) };
            true
        } else if self.weak.remove(&raw) {
            // gc-common w8-c: a weak entry (`DeleteWeakGlobalRef`, or a
            // native that deletes a weak handle through `DeleteGlobalRef`).
            // Safety: raw was created by Box::into_raw (a Box<usize>) and we
            // own it.
            unsafe { drop(Box::from_raw(raw as *mut usize)) };
            true
        } else {
            tracing::debug!("JNI remove: global ref handle {handle:#x} not found");
            false
        }
    }

    /// Resolve a global ref handle to the referenced ObjectRef.
    /// Returns `None` if `handle` is 0 or is not a valid global ref.
    pub fn resolve(&self, handle: JObject) -> Option<ObjectRef> {
        if handle == 0 || handle & 1 == 0 {
            return None;
        }
        let raw = (handle & !1) as usize;
        // Validate the pointer is still tracked to prevent use-after-free.
        if !self.entries.contains(&raw) {
            // gc-common w8-c: a weak entry resolves to its referent, or to
            // NULL once the referent was collected (the handle stays valid).
            if self.weak.contains(&raw) {
                // Safety: raw is a Box<usize> pointer still in self.weak.
                let addr = unsafe { *(raw as *const usize) };
                if addr == 0 {
                    return None;
                }
                // Safety: a non-zero slot is the referent's current address,
                // kept current by every sweep since it was stored.
                return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
            }
            tracing::warn!("JNI resolve: handle {handle:#x} not found in global ref table");
            return None;
        }
        // Safety: raw was created by Box::into_raw and is still in self.entries.
        Some(unsafe { *(raw as *const ObjectRef) })
    }

    /// How many active global refs exist.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Collect all referenced ObjectRefs into `out` (for GC root scanning).
    pub fn collect_roots(&self, out: &mut Vec<ObjectRef>) {
        for &raw in &self.entries {
            // Safety: raw is a valid Box<ObjectRef> pointer that we own.
            out.push(unsafe { *(raw as *const ObjectRef) });
        }
    }

    /// Apply a GC pointer map: update all stored ObjectRefs to their new addresses.
    pub fn update_after_gc(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        for &raw in &self.entries {
            // Safety: raw is a valid Box<ObjectRef> pointer that we own.
            let slot = raw as *mut ObjectRef;
            let old_obj = unsafe { *slot };
            let old_addr = old_obj.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                unsafe { *slot = ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

impl Default for JniGlobalRefs {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JniGlobalRefs {
    fn drop(&mut self) {
        for raw in self.entries.drain() {
            // Safety: raw was created by Box::into_raw and we own it.
            unsafe { drop(Box::from_raw(raw as *mut ObjectRef)) };
        }
        for raw in self.weak.drain() {
            // Safety: raw was created by Box::into_raw (a Box<usize>) and we
            // own it.
            unsafe { drop(Box::from_raw(raw as *mut usize)) };
        }
    }
}

// ---------------------------------------------------------------------------
// Weak global refs: the post-collection sweeps (gc-common w8-c)
// ---------------------------------------------------------------------------

/// Add every uncleared weak global's referent to the collecting thread's
/// WATCHED set for the collection about to run.
///
/// Call it on the collecting thread AFTER `weakref_null_referents_pre_gc`
/// (which REPLACES the set) and before the collection. It is what makes
/// [`sweep_weak_global_refs`]'s verdict exact on a Generational cycle that
/// reclaims old-gen storage: the old-gen paths emit an identity pointer-map
/// entry only for a WATCHED survivor that did not move, and
/// `VmHeap::watched_pre_gc_addr_survived` judges an unwatched, unmoved
/// old-gen address dead once old-gen storage was reclaimed. Without this, a
/// live, unmoved old-gen referent would read as NULL after a major cycle
/// (never the reverse: the verdict cannot keep a dead referent). Watching an
/// address never keeps it alive; the set only answers "did it survive".
///
/// Costs nothing when no weak global exists (one lock, one empty check).
pub(crate) fn watch_weak_global_referents(shared: &SharedVm) {
    let addrs: Vec<usize> = {
        let refs = shared.natives.jni_global_refs.lock();
        if refs.weak.is_empty() {
            return;
        }
        // Straight into the address list (gc-common w10-c): no intermediate
        // `(box, address)` snapshot on every pause.
        refs.weak
            .iter()
            // Safety: every `raw` is a `Box<usize>` this table owns, read
            // under its lock.
            .map(|&raw| unsafe { *(raw as *const usize) })
            .filter(|&addr| addr != 0)
            .collect()
    };
    if addrs.is_empty() {
        return;
    }
    // Extend in place (gc-common w9-g): this used to clone the whole set, copy
    // it into a Vec and rebuild it to add these few addresses, on every pause.
    cratonvm_gc::gc_quiescence::add_watched_referents(&addrs);
}

/// Remap or clear every weak global ref after a STOP-THE-WORLD collection
/// (gc-common w8-c, `common-w7c-jni-weak-global-refs-are-strong`).
///
/// For each uncleared weak entry, with the PRE-collection address:
///
/// * in `pointer_map`: the referent moved (or is an identity survivor); the
///   entry now names the new address;
/// * else survived in place: kept. The verdict is the reference processor's
///   (`VmHeap::watched_pre_gc_addr_survived`, see
///   `process_references_after_gc`) AND `addr_keyed::survived_in_place`, so a
///   header that still parses in reclaimed space never keeps an entry;
/// * else: the referent died, the entry is CLEARED. From here on
///   `IsSameObject(wref, NULL)` answers `JNI_TRUE` and `NewLocalRef(wref)`
///   answers NULL; the handle itself stays valid until `DeleteWeakGlobalRef`.
///
/// Finalization: a finalizable referent is resurrected by the collection
/// (`finalizable_roots`), so it survives this verdict and the weak global
/// keeps naming it until the object is really gone. That is the JNI spec's
/// phantom strength for weak globals (JDK 9+), not the weak strength of a
/// `java.lang.ref.WeakReference`.
///
/// Where it must run: inside the pause, after the collection and before the
/// world is released (the verdict is only meaningful before an allocation can
/// reuse a reclaimed address), on EVERY stop-the-world collection of every
/// backend -- `run_collection_pause`, which also calls
/// [`watch_weak_global_referents`] before the collection (gc-common w8-c
/// handoff `handoff-w8c-jni-weak-global-sweep-call-sites`).
/// Reclamation outside a collection pause is covered elsewhere: the
/// concurrent remarks by [`clear_weak_global_refs_unmarked`], and the
/// Generational concurrent sweep and G1 cleanup by the address-keyed sweeps
/// (`addr_keyed::sweep_address_keyed_tables`,
/// `addr_keyed::drop_address_keyed_rows_in`).
///
/// No table lock is held while the heap is asked (the survival predicates
/// take the old-gen lock, and the span drop takes this table's lock UNDER
/// the old-gen lock); the verdicts are applied afterwards, per entry, only
/// if the entry still holds the address that was judged.
///
/// Test-only since gc-common w18 (`handoff-w18a-in-pause-sweeps-take-the-in-place-verdict`):
/// the pause calls [`sweep_weak_global_refs_with`] with its batched
/// `InPlaceVerdict`; this per-address form stays for the tests.
#[cfg(test)]
pub(crate) fn sweep_weak_global_refs(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> WeakGlobalSweepStats {
    let heap = &shared.mem.heap;
    sweep_weak_global_refs_with(shared, pointer_map, &|addr| {
        crate::memory::addr_keyed::survived_in_place(heap, addr)
    })
}

/// [`sweep_weak_global_refs`] with the caller's in-place survival verdict
/// (`run_collection_pause`: a once-per-pause `addr_keyed::InPlaceVerdict`,
/// gc-common w18-a).
pub(crate) fn sweep_weak_global_refs_with(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> WeakGlobalSweepStats {
    let snapshot = shared.natives.jni_global_refs.lock().weak_snapshot();
    if snapshot.is_empty() {
        return WeakGlobalSweepStats::default();
    }
    let heap = &shared.mem.heap;
    let mut stats = WeakGlobalSweepStats::default();
    let mut verdicts: Vec<(usize, usize, usize)> = Vec::with_capacity(snapshot.len());
    for (raw, addr) in snapshot {
        let now = match pointer_map.get(&addr) {
            Some(&to) => {
                if to != addr {
                    stats.moved += 1;
                } else {
                    stats.retained += 1;
                }
                to
            }
            None if heap.watched_pre_gc_addr_survived(addr, pointer_map) && survived(addr) => {
                stats.retained += 1;
                addr
            }
            None => {
                stats.cleared += 1;
                0
            }
        };
        if now != addr {
            verdicts.push((raw, addr, now));
        }
    }
    if !verdicts.is_empty() {
        shared
            .natives
            .jni_global_refs
            .lock()
            .apply_weak_verdicts(&verdicts);
    }
    stats
}

/// Clear every weak global ref whose referent the concurrent cycle's COMPLETED
/// mark judged dead, at the remark, before anything is freed (gc-common w8-c).
///
/// `is_marked` is the collector's remark verdict over current addresses
/// (G1: bitmap + TAMS, `g1_remark_process_references`; Generational: bitmap,
/// or not sweep-eligible, `ConcurrentMarker::remark_with_reference_processing`).
/// Nothing moves in a remark, so there is no remap. `keep` lists addresses the
/// same pause is about to resurrect (the finalizable objects the reference
/// pass returns); their weak globals are kept, the phantom strength
/// [`sweep_weak_global_refs`] documents. An object reachable ONLY through such
/// a resurrected object is not in `keep` and is cleared here, one cycle
/// earlier than HotSpot would: safe (the handle reads NULL), and rare.
///
/// Why at the remark and not after the reclamation: after the remark the
/// SATB barrier is off, so a native that resolved a dead referent between the
/// remark and the free would hold a local the concurrent sweep then frees
/// (Generational), or read through it into a region cleanup freed (G1: a
/// dead object in a partly live region outlives the cycle, its dead
/// neighbours do not). Clearing here makes the handle NULL before any mutator
/// runs again. Call sites: the handoff named at [`sweep_weak_global_refs`].
pub(crate) fn clear_weak_global_refs_unmarked(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    keep: &[usize],
) -> usize {
    let snapshot = shared.natives.jni_global_refs.lock().weak_snapshot();
    if snapshot.is_empty() {
        return 0;
    }
    let verdicts: Vec<(usize, usize, usize)> = snapshot
        .into_iter()
        .filter(|&(_, addr)| !is_marked(addr) && !keep.contains(&addr))
        .map(|(raw, addr)| (raw, addr, 0))
        .collect();
    if !verdicts.is_empty() {
        shared
            .natives
            .jni_global_refs
            .lock()
            .apply_weak_verdicts(&verdicts);
    }
    verdicts.len()
}

/// The out-of-pause half: clear every weak global whose referent is no longer
/// a live object in place, with an EMPTY pointer map (nothing moved). Driven by
/// `addr_keyed::sweep_address_keyed_tables` after the reclamations that are
/// not followed by a collection epilogue (the Generational concurrent sweep,
/// G1's remark cleanup). Same verdict as the other address-keyed sweeps
/// there: `addr_keyed::survived_in_place`.
pub(crate) fn sweep_weak_global_refs_in_place(shared: &SharedVm) -> usize {
    let snapshot = shared.natives.jni_global_refs.lock().weak_snapshot();
    if snapshot.is_empty() {
        return 0;
    }
    let heap = &shared.mem.heap;
    let verdicts: Vec<(usize, usize, usize)> = snapshot
        .into_iter()
        .filter(|&(_, addr)| !crate::memory::addr_keyed::survived_in_place(heap, addr))
        .map(|(raw, addr)| (raw, addr, 0))
        .collect();
    if !verdicts.is_empty() {
        shared
            .natives
            .jni_global_refs
            .lock()
            .apply_weak_verdicts(&verdicts);
    }
    verdicts.len()
}

/// Clear every weak global whose referent lies in one of `spans` (freed spans,
/// sorted by start, non-overlapping). A pure range test, safe UNDER the
/// old-gen guard: it takes only this table's lock, a leaf that is never held
/// while the heap is asked (see [`sweep_weak_global_refs`]). Driven by
/// `addr_keyed::drop_address_keyed_rows_in`.
pub(crate) fn clear_weak_global_refs_in_spans(
    shared: &SharedVm,
    spans: &[(usize, usize)],
) -> usize {
    if spans.is_empty() {
        return 0;
    }
    let refs = shared.natives.jni_global_refs.lock();
    if refs.weak.is_empty() {
        return 0;
    }
    // In place, under the one lock hold (gc-common w10-c): this used to copy
    // every uncleared weak entry into a snapshot `Vec`, filter it into a
    // second, then walk the set again to apply them, for a verdict that needs
    // no lock released in between (a pure range test).
    let mut cleared = 0usize;
    for &raw in &refs.weak {
        let slot = raw as *mut usize;
        // Safety: `raw` is a `Box<usize>` this table owns; the table lock is
        // held, and every other access to a weak slot takes it.
        unsafe {
            let addr = *slot;
            if addr != 0 && crate::memory::addr_keyed::in_spans(addr, spans) {
                *slot = 0;
                cleared += 1;
            }
        }
    }
    cleared
}

// ---------------------------------------------------------------------------
// Local Reference Frame Stack
// ---------------------------------------------------------------------------
//
// JNI local refs are raw heap pointers (bit 0 = 0). Native code can call
// PushLocalFrame/PopLocalFrame to scope their lifetime. We maintain a per-
// thread stack of frames in JNI TLS; each frame is a Vec<JObject>.
//
// Thread-local local frame stack — used only while JNI is active.

thread_local! {
    static JNI_LOCAL_FRAMES: std::cell::RefCell<Vec<Vec<JObject>>> =
        std::cell::RefCell::new(Vec::new());
}

thread_local! {
    /// gc-common w7-c: the depths (indices into [`JNI_LOCAL_FRAMES`]) of the
    /// frames NATIVE CODE opened with `PushLocalFrame`, ascending. Every other
    /// frame is one the VM opened around a call (the dispatch's implicit
    /// frame, a foreign thread's per-call frame), and `PopLocalFrame` must
    /// never pop one of those: see [`jni_pop_local_frame`].
    static JNI_EXPLICIT_FRAME_DEPTHS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The most slots [`push_local_frame`] reserves up front. A frame grows on
/// demand past it; the reservation is only a hint.
const LOCAL_FRAME_RESERVE_MAX: usize = 4096;

/// Push a new local frame onto this thread's local frame stack.
///
/// gc-common w10-c: `capacity` is a hint, reserved up to
/// [`LOCAL_FRAME_RESERVE_MAX`]. It came straight from `PushLocalFrame`, and
/// `Vec::with_capacity` of a native's `PushLocalFrame(env, INT_MAX)` is a 16 GiB
/// reservation that aborts the process instead of answering.
pub fn push_local_frame(capacity: usize) {
    let want = capacity.clamp(16, LOCAL_FRAME_RESERVE_MAX);
    // gcd d10/j: reuse the storage of the last closed frame when it is big
    // enough (every JNI native dispatch opens and closes one implicit frame).
    let frame = take_spare_local_frame(want).unwrap_or_else(|| Vec::with_capacity(want));
    JNI_LOCAL_FRAMES.with(|f| {
        f.borrow_mut().push(frame);
    });
}

/// gcd d10/j: the largest frame storage (in slots) kept for reuse; a bigger
/// one is freed as before, so a native that once minted thousands of locals
/// does not pin that much memory for the thread's life.
const SPARE_LOCAL_FRAME_MAX: usize = 64;

/// The spare frame storage ([`NativeCallTls::spare_local_frame`]) when it has
/// room for `want` slots. Always empty.
fn take_spare_local_frame(want: usize) -> Option<Vec<JObject>> {
    JNI_NATIVE_CALL
        .try_with(|t| {
            let spare = t.spare_local_frame.take();
            if spare.capacity() >= want {
                Some(spare)
            } else {
                // Too small for this frame: keep it for the next one.
                t.spare_local_frame.set(spare);
                None
            }
        })
        .ok()
        .flatten()
}

/// Keep `frame`'s storage for the next [`push_local_frame`], emptied, when no
/// storage is kept yet and it is not oversized; otherwise drop it.
fn keep_spare_local_frame(mut frame: Vec<JObject>) {
    if frame.capacity() == 0 || frame.capacity() > SPARE_LOCAL_FRAME_MAX {
        return;
    }
    frame.clear();
    let _ = JNI_NATIVE_CALL.try_with(|t| {
        let current = t.spare_local_frame.take();
        if current.capacity() >= frame.capacity() {
            t.spare_local_frame.set(current);
        } else {
            t.spare_local_frame.set(frame);
        }
    });
}

/// How many local frames this thread has open. A VM-side scope records it
/// before opening its frame and hands it back to [`truncate_local_frames`]
/// when the scope ends.
pub fn local_frame_depth() -> usize {
    JNI_LOCAL_FRAMES
        .try_with(|f| f.try_borrow().map_or(0, |s| s.len()))
        .unwrap_or(0)
}

/// Close every local frame at or above `depth`: the frame a VM-side scope
/// opened at that depth, and any frame native code pushed inside it and never
/// popped.
///
/// gc-common w7-c. JNI frees a native method's local frames when it returns,
/// the ones it pushed and leaked included (HotSpot's native wrapper restores
/// the thread's handle block). Popping ONE frame at scope exit, as the scopes
/// did, popped the leaked frame and left the scope's own frame open: a root
/// for everything in it for the life of the thread, and one more frame depth
/// every later native saw.
pub fn truncate_local_frames(depth: usize) {
    // `try_with`: a scope can end during thread teardown, after this
    // thread's TLS is gone; there is nothing left to close then.
    let closed = JNI_LOCAL_FRAMES
        .try_with(|f| {
            let Ok(mut stack) = f.try_borrow_mut() else {
                return None;
            };
            if stack.len().checked_sub(1) == Some(depth) {
                // The common case -- a dispatch closing its one implicit
                // frame: its storage may be reused (gcd d10/j).
                return stack.pop();
            }
            stack.truncate(depth);
            None
        })
        .ok()
        .flatten();
    if let Some(frame) = closed {
        keep_spare_local_frame(frame);
    }
    forget_explicit_frames_from(depth);
}

/// Drop the explicit-frame records at or above `depth` (their frames are gone).
fn forget_explicit_frames_from(depth: usize) {
    let _ = JNI_EXPLICIT_FRAME_DEPTHS.try_with(|d| {
        if let Ok(mut depths) = d.try_borrow_mut() {
            while depths.last().is_some_and(|&x| x >= depth) {
                depths.pop();
            }
        }
    });
}

/// Is the innermost open local frame one native code pushed with
/// `PushLocalFrame` (as opposed to a frame the VM opened around a call)?
fn top_local_frame_is_explicit() -> bool {
    let depth = local_frame_depth();
    depth > 0
        && JNI_EXPLICIT_FRAME_DEPTHS
            .try_with(|d| d.try_borrow().is_ok_and(|v| v.last() == Some(&(depth - 1))))
            .unwrap_or(false)
}

/// Pop the topmost local frame, returning `result` promoted to the parent
/// frame.
///
/// gc-common w4-g: "promoted" used to be a comment only -- the result was
/// handed back and recorded nowhere, so an object the native built inside a
/// `PushLocalFrame` scope and returned through `PopLocalFrame` (the documented
/// idiom for keeping one result while dropping the scope's temporaries) was
/// named by no root from the pop on: collectable under the native, and not
/// rewritten by a moving collection. It is now recorded in the new innermost
/// frame (`record_local_handle`: a no-op for 0, a global ref, a `jclass`, or
/// when no parent frame is open).
pub fn pop_local_frame(result: JObject) -> JObject {
    // gc-common w6-g (`CRATONVM_JNI_INDIRECT_LOCALS`): an indirect `result`
    // names a slot, most often of the frame being popped, so it is read
    // BEFORE the pop and handed back as a NEW handle in the parent frame.
    if let Some(slot) = decode_indirect_local(result) {
        let raw = read_local_slot(slot);
        let depth = JNI_LOCAL_FRAMES.with(|f| {
            let mut stack = f.borrow_mut();
            let _ = stack.pop();
            stack.len()
        });
        forget_explicit_frames_from(depth);
        return raw.map_or(0, record_local_handle_indirect);
    }
    let depth = JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        let _ = stack.pop(); // drop all refs in the top frame
        stack.len()
    });
    forget_explicit_frames_from(depth);
    if indirect_locals_active() {
        return record_local_handle_indirect(result);
    }
    record_local_handle(result);
    result
}

/// Record a local ref in the current top frame.
///
/// GC-correctness (vm-jni-roots #2): previously, if there was no active frame
/// the ref was silently DROPPED ("untracked") and therefore was NOT a GC root —
/// a local ref a native obtained (NewObject, GetObjectField, …) outside any
/// explicit `PushLocalFrame` was invisible to `collect_local_ref_roots` and
/// could be reclaimed (or left dangling under a moving GC) mid-native-call.
/// The native dispatch path now pushes an IMPLICIT top-level local frame for
/// the duration of every JNI native call (see `vm_exec`), but we additionally
/// synthesize a frame here so a `track_local_ref` that races ahead of (or runs
/// without) an enclosing frame still roots the handle rather than leaking it.
pub fn track_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if stack.last().is_none() {
            // No active frame: synthesize an implicit top-level frame so the
            // handle is tracked (and thus a GC root) instead of being dropped.
            stack.push(Vec::with_capacity(16));
        }
        // Safe: we just ensured a top frame exists.
        if let Some(top) = stack.last_mut() {
            top.push(jobj);
        }
    });
}

/// Remove ONE local ref from the current top frame (for DeleteLocalRef).
///
/// gc-common w4-g: this removed EVERY entry equal to `jobj`. A local ref is the
/// object's raw address, so two local refs to one object -- a native that read
/// the same field twice, or got the same object back from two up-calls -- are
/// two equal entries, and deleting one unrooted the other while the native
/// still held it. Only the most recent matching entry goes now.
pub fn delete_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    // gc-common w6-g: an indirect handle clears exactly its own slot. With
    // indirect handles live, no entry may be REMOVED from a frame (that would
    // shift every later slot's index under the handles naming them), so the
    // raw arm below clears too.
    if let Some((depth, idx)) = decode_indirect_local(jobj) {
        JNI_LOCAL_FRAMES.with(|f| {
            if let Some(frame) = f.borrow_mut().get_mut(depth) {
                clear_local_slot(frame, idx);
            }
        });
        return;
    }
    let indirect = indirect_locals_active();
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if let Some(top) = stack.last_mut() {
            if let Some(i) = top.iter().rposition(|&r| r == jobj) {
                if indirect {
                    clear_local_slot(top, i);
                } else {
                    top.remove(i);
                }
            }
        }
    });
}

/// Zero `frame[idx]` (a deleted local ref), then drop every trailing zeroed
/// slot, which makes the common `New*` + `DeleteLocalRef` loop reuse its slot
/// instead of growing the frame by one word per iteration. Only TRAILING
/// slots go: removing an interior one would move the slots after it. A
/// non-LIFO delete leaves a zero hole until the frame is popped (the roots
/// and remap walks already skip 0).
fn clear_local_slot(frame: &mut Vec<JObject>, idx: usize) {
    if let Some(v) = frame.get_mut(idx) {
        *v = 0;
    }
    while frame.last() == Some(&0) {
        frame.pop();
    }
}

/// Collect every active JNI **local** reference held by THIS thread into `out`
/// for GC root scanning.
///
/// BUG FIX (vm-jni-roots #1): the per-thread `JNI_LOCAL_FRAMES` stack holds
/// local-ref `JObject` handles, which for local refs are raw heap pointers
/// (bit 0 = 0). Previously only `JniGlobalRefs::collect_roots` was folded into
/// the root set (see `roots.rs`), so a heap object reachable ONLY through a JNI
/// local ref could be reclaimed mid-native-call (or, under a moving GC, left as
/// a stale from-space pointer). This mirrors `JniGlobalRefs::collect_roots`,
/// but for the thread-local local-frame stack. Must be called on each thread
/// that may hold local refs (the per-thread `roots::collect_roots`).
///
/// Global refs (bit 0 = 1) never appear in `JNI_LOCAL_FRAMES`, but we defend
/// against a tagged value sneaking in by skipping any handle with bit 0 set
/// (those are rooted separately via `JniGlobalRefs`).
pub fn collect_local_ref_roots(out: &mut Vec<ObjectRef>) {
    JNI_LOCAL_FRAMES.with(|f| {
        for frame in f.borrow().iter() {
            for &handle in frame.iter() {
                if handle == 0 || handle & 1 == 1 {
                    continue;
                }
                // Safety: a local ref is a raw, non-null heap pointer; it was a
                // live ObjectRef when pushed and is kept live precisely by being
                // reported here.
                out.push(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    });
}

/// gcd d10/j: the extent of this thread's local frames at one instant -- how
/// many frames were open and how many slots the innermost one had -- so that
/// [`collect_local_ref_roots_since`] can name exactly the locals minted after
/// it (a JNIEnv function only pushes onto the innermost frame, or opens one).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct LocalFramesMark {
    depth: usize,
    top_len: usize,
}

/// This thread's [`LocalFramesMark`] now. `Default` (everything counts as
/// new) when the table is unreadable.
fn local_frames_mark() -> LocalFramesMark {
    JNI_LOCAL_FRAMES
        .try_with(|f| {
            f.try_borrow().map_or(LocalFramesMark::default(), |stack| LocalFramesMark {
                depth: stack.len(),
                top_len: stack.last().map_or(0, Vec::len),
            })
        })
        .unwrap_or_default()
}

/// [`collect_local_ref_roots`] for the locals minted since `mark`: every entry
/// of a frame opened after it, and the entries of the frame that was innermost
/// at `mark` from its then-length on. Frames below are skipped (their entries
/// were published before). Same filters as `collect_local_ref_roots`.
fn collect_local_ref_roots_since(mark: LocalFramesMark, out: &mut Vec<ObjectRef>) {
    let _ = JNI_LOCAL_FRAMES.try_with(|f| {
        let Ok(stack) = f.try_borrow() else {
            return;
        };
        for (index, frame) in stack.iter().enumerate() {
            let from = match (index + 1).cmp(&mark.depth) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => mark.top_len,
                std::cmp::Ordering::Greater => 0,
            };
            for &handle in frame.iter().skip(from) {
                if handle == 0
                    || handle & 1 == 1
                    || is_jclass_handle(handle)
                    || is_indirect_local_tag(handle)
                {
                    continue;
                }
                // Safety: as in `collect_local_ref_roots`.
                out.push(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    });
}

/// gcd d4/k (for lane m's JNI-local-aware pin in the pinned young copy): call
/// `f` with every live JNI local reference held in THIS thread's local frames,
/// innermost frame last, without allocating. Read-only; the same entries
/// [`collect_local_ref_roots`] roots (deleted slots, global refs, `jclass`
/// tags and indirect tags skipped). Thread-local storage, so it answers only
/// for the calling thread: a peer's locals reach a collection through its
/// deposited root snapshot. A no-op during TLS teardown or while the frame
/// table is borrowed (never, outside a local-frame walk).
///
/// Test-only until the proposal that pins JNI local referents in the young
/// pin ledger lands its first production caller (gcd d4/m); drop the `cfg`
/// then (`vm/tests/no_test_only_public_api.rs`).
#[cfg(test)]
pub(crate) fn for_each_live_local_ref(mut f: impl FnMut(ObjectRef)) {
    let _ = JNI_LOCAL_FRAMES.try_with(|frames| {
        let Ok(stack) = frames.try_borrow() else {
            return;
        };
        for frame in stack.iter() {
            for &handle in frame.iter() {
                if handle == 0
                    || handle & 1 == 1
                    || is_jclass_handle(handle)
                    || is_indirect_local_tag(handle)
                {
                    continue;
                }
                // Safety: a stored local is a raw, non-null, 8-aligned heap
                // address, kept live by this very table (see
                // `collect_local_ref_roots`).
                f(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    });
}

/// gcd d4/k (lane m's companion question): may native code on THIS thread
/// hold a RAW copy of one of its locals, i.e. an address a move would leave
/// stale in the native's own variables? `true` unless every local handed out
/// here was an indirect handle ([`locals_are_indirect_here`]) and none escaped
/// raw ([`raw_local_escapes`], monotonic per thread, so the answer is
/// conservative). With the default flags it is `true`: pin rather than move.
pub(crate) fn local_refs_may_be_held_raw() -> bool {
    !locals_are_indirect_here() || raw_local_escapes() != 0
}

/// Apply a GC pointer map to every active JNI local reference on THIS thread,
/// rewriting moved-object handles in place.
///
/// BUG FIX (vm-jni-roots #1): after a moving GC relocates objects, the raw
/// pointers stored in `JNI_LOCAL_FRAMES` would dangle (point at from-space).
/// This mirrors `JniGlobalRefs::update_after_gc` for the local-frame stack:
/// for each stored local-ref handle whose address appears in `pointer_map`, we
/// overwrite it with the relocated address so the native code's local jobject
/// continues to resolve to the live object. Must be called on each thread that
/// may hold local refs, after the heap has been compacted.
pub fn update_local_refs_after_gc(pointer_map: &cratonvm_types::PointerMap) {
    JNI_LOCAL_FRAMES.with(|f| {
        for frame in f.borrow_mut().iter_mut() {
            for handle in frame.iter_mut() {
                if *handle == 0 || *handle & 1 == 1 {
                    continue;
                }
                let old_addr = *handle as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    *handle = new_addr as JObject;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// The raw handle of `obj`: a pure conversion, recorded nowhere. Use it to
/// COMPARE or to key a handle; a local ref HANDED TO native code must come from
/// [`new_local_handle`], or the native holds an object no root names.
pub fn obj_to_jobject(obj: ObjectRef) -> JObject {
    obj.as_ptr() as u64
}

/// A LOCAL ref handed to native code (a JNI function's result, a native
/// method's receiver or argument): the raw handle, recorded in this thread's
/// innermost open local frame.
///
/// gc-common w3-g. `JniImplicitFrameGuard`'s doc (vm_exec) says every local
/// ref a native obtains "is tracked via `track_local_ref`" and is therefore a
/// GC root (`collect_local_ref_roots`) and remapped
/// (`update_local_refs_after_gc`) for the duration of the call. Only
/// `NewLocalRef` actually called it: `NewStringUTF`, `NewObject`/`AllocObject`,
/// `New<Type>Array`, `Get{Object,StaticObject}Field`,
/// `GetObjectArrayElement` and every `Call*ObjectMethod*` returned a bare
/// address that no root named. A native that created a string and then made an
/// allocating up-call before using it could have that string collected under
/// it, on every backend. Recording the handle here closes the LIVENESS half;
/// the native's own copy of the address is still not rewritten by a moving
/// collection (`docs/known-issues/gc/common-w2c-jni-local-refs-are-raw-addresses.md`).
///
/// Recorded only when a frame is OPEN (a native dispatch's implicit frame or
/// an explicit `PushLocalFrame`). A foreign-attached thread calling JNI
/// functions outside any native method has no frame to scope the ref to; the
/// handle is returned unrecorded there, exactly as before, rather than
/// synthesizing a frame nothing would ever pop (a root leak for the life of
/// the thread -- `track_local_ref` does synthesize one, for `NewLocalRef`).
/// With `CRATONVM_JNI_FOREIGN_TRANSITIONS` on (gc-common w10-c) such a thread
/// has its attach-level frame, which detach pops, and the handle is recorded
/// there.
///
/// Cost: one TLS access and a `Vec` push per local ref handed out; the frame
/// is dropped wholesale when the native returns.
pub fn new_local_handle(obj: ObjectRef) -> JObject {
    let h = obj_to_jobject(obj);
    if indirect_locals_active() {
        return record_local_handle_indirect(h);
    }
    record_local_handle(h);
    h
}

// ---------------------------------------------------------------------------
// Indirect local handles (`CRATONVM_JNI_INDIRECT_LOCALS`, default OFF)
// ---------------------------------------------------------------------------
//
// gc-common w6-g, `docs/known-issues/gc/common-w2c-jni-local-refs-are-raw-addresses.md`.
// With the flag on, a local ref handed to native code inside an open frame is
// not the object's address but the address of nothing: a tagged `(frame
// depth, slot)` pair naming the entry of `JNI_LOCAL_FRAMES` that holds the
// address. The table is already a root and already rewritten by every moving
// collection (`collect_local_ref_roots`, `update_local_refs_after_gc`), so a
// native that keeps its `jobject` across an up-call that lets a collection
// move the object now reads the object's CURRENT address at its next use
// (`jobject_to_obj`), like HotSpot's handle-block locals.
//
// What stays raw, exactly as with the flag off: a handle created with no open
// frame (nothing would pop a synthesized one), every handle on a
// foreign-attached thread (its per-call frame is popped at the end of EACH
// JNI call, while its natives keep locals across calls) unless
// `CRATONVM_JNI_FOREIGN_TRANSITIONS` gives it an attach-level frame (gc-common
// w10-c), and a slot past the encodable range. Decoding accepts both forms.
//
// Behaviour a JNI library can see (why the flag defaults OFF until a
// JNI-library run, e.g. netty-tcnative, JNA, lz4/zstd-jni, has used it):
// * a local ref is valid only on its own thread and only until its frame is
//   popped, as the JNI spec says. A library that passes a local to another
//   thread, or keeps one past its native's return (both undefined behaviour,
//   and both "work" with a raw address until a collection moves the object),
//   now resolves NULL or another slot's object;
// * a local smuggled through a `jlong` return is no longer an object address,
//   so the `J`-return mint chokepoint (`smuggled_longs`) does not register it.
// Frame reuse (a stale `(depth, slot)` naming a later frame's entry) is the
// same undefined behaviour HotSpot has.

/// The tag an indirect local handle carries in bits 48..=63. Neither a
/// user-space address (canonical user addresses are below `1 << 47`) nor a
/// `jclass` (`JCLASS_TAG`, `0x7F51` in the same bits) can carry it.
const INDIRECT_LOCAL_TAG: JObject = 0x7F52 << 48;
/// Selects [`INDIRECT_LOCAL_TAG`]'s bits.
const INDIRECT_LOCAL_TAG_MASK: JObject = 0xFFFF << 48;
/// The largest frame depth an indirect handle can name (bits 32..=47).
const INDIRECT_LOCAL_MAX_DEPTH: usize = 0xFFFF;
/// The largest slot index an indirect handle can name (bits 1..=31; bit 0 is
/// clear so the handle never reads as a global ref).
const INDIRECT_LOCAL_MAX_SLOT: usize = (1 << 31) - 1;

/// `CRATONVM_JNI_INDIRECT_LOCALS`, latched like every flag. Default OFF;
/// gcd d10/j: also on with the package switch `CRATONVM_JNI_NATIVE_TRANSITIONS`
/// unless set to `0` ([`jni_switches`]).
fn indirect_locals_active() -> bool {
    if let Some(on) = indirect_locals_test_override() {
        return on;
    }
    jni_switches().indirect_locals
}

/// The value rule of [`indirect_locals_active`]: unset, empty, or an off word
/// (`0` / `false` / `off` / `no`) is OFF; anything else (the grouped
/// `CRATONVM_GC=jni-indirect-locals` spelling included) is ON.
fn indirect_locals_from(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim),
        None | Some("" | "0" | "false" | "off" | "no")
    )
}

#[cfg(test)]
thread_local! {
    /// Unit tests switch the indirection per thread: the flag is latched per
    /// process, and tests share one.
    static INDIRECT_LOCALS_TEST_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

#[cfg(test)]
fn indirect_locals_test_override() -> Option<bool> {
    INDIRECT_LOCALS_TEST_OVERRIDE.with(Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn indirect_locals_test_override() -> Option<bool> {
    None
}

/// Encode `(depth, slot)` as an indirect local handle, if both fit.
fn encode_indirect_local(depth: usize, slot: usize) -> Option<JObject> {
    (depth <= INDIRECT_LOCAL_MAX_DEPTH && slot <= INDIRECT_LOCAL_MAX_SLOT)
        .then(|| INDIRECT_LOCAL_TAG | ((depth as JObject) << 32) | ((slot as JObject) << 1))
}

/// Does `h` carry the indirect-local encoding (whether or not the flag is on)?
fn is_indirect_local_tag(h: JObject) -> bool {
    h & INDIRECT_LOCAL_TAG_MASK == INDIRECT_LOCAL_TAG && h & 1 == 0
}

/// The `(depth, slot)` an indirect local handle names. `None` for every other
/// handle, and for every handle while the flag is off (a forged tagged value
/// then takes the raw path and fails its heap check, as before).
fn decode_indirect_local(h: JObject) -> Option<(usize, usize)> {
    if !is_indirect_local_tag(h) || !indirect_locals_active() {
        return None;
    }
    Some((
        ((h >> 32) & 0xFFFF) as usize,
        ((h & 0xFFFF_FFFF) >> 1) as usize,
    ))
}

/// The raw address currently in local slot `(depth, slot)`, if the slot
/// exists on this thread and was not deleted.
fn read_local_slot((depth, slot): (usize, usize)) -> Option<JObject> {
    JNI_LOCAL_FRAMES.with(|f| {
        let stack = f.try_borrow().ok()?;
        stack.get(depth)?.get(slot).copied().filter(|&v| v != 0)
    })
}

/// Record the raw local `h` in the innermost open frame and return the handle
/// to give native code: an indirect handle naming that slot when the flag is
/// on, the thread is not foreign-attached and the slot is encodable;
/// otherwise `h` itself (recorded when a frame is open, exactly as
/// [`record_local_handle`] does). A global ref, a `jclass`, an already
/// indirect handle and 0 are returned unchanged and not recorded.
fn record_local_handle_indirect(h: JObject) -> JObject {
    if h == 0 || h & 1 == 1 || is_jclass_handle(h) || is_indirect_local_tag(h) {
        return h;
    }
    // A foreign-attached thread stays raw unless its attach-level locals live
    // in a frame nothing pops before detach (gc-common w10-c,
    // `CRATONVM_JNI_FOREIGN_TRANSITIONS`): then a slot names them for as long
    // as the native may hold them, as on any other thread.
    let encode = locals_are_indirect_here();
    let handed = JNI_LOCAL_FRAMES.with(|f| {
        let Ok(mut stack) = f.try_borrow_mut() else {
            return h;
        };
        let Some(depth) = stack.len().checked_sub(1) else {
            return h;
        };
        let top = &mut stack[depth];
        let slot = top.len();
        top.push(h);
        if encode {
            encode_indirect_local(depth, slot).unwrap_or(h)
        } else {
            h
        }
    });
    // gcd d3/k: a raw handle where an indirect one was due -- no open frame,
    // or a slot past the encodable range -- is one no moving collection may
    // outlive while the native holds it (`JniNativeCall`).
    if encode && handed == h {
        note_raw_local_escape();
    }
    handed
}

/// Record an existing raw local handle (bit 0 clear, not a `jclass` tag) in the
/// innermost open local frame; see [`new_local_handle`]. A no-op with no open
/// frame, for a global ref / `jclass` handle, and if the frame stack is already
/// borrowed on this thread (never expected: no JNI function runs inside a
/// local-frame walk).
fn record_local_handle(h: JObject) {
    if h == 0 || h & 1 == 1 || is_jclass_handle(h) {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        if let Ok(mut stack) = f.try_borrow_mut() {
            if let Some(top) = stack.last_mut() {
                top.push(h);
            }
        }
    });
}

/// Where a handle [`new_local_handle`] JUST recorded lives: `(frame depth,
/// index)` of the innermost frame's last entry, when that entry is `h`.
/// `None` when nothing was recorded (no open frame).
fn recorded_local_slot_of(h: JObject) -> Option<(usize, usize)> {
    JNI_LOCAL_FRAMES.with(|f| {
        let stack = f.try_borrow().ok()?;
        let depth = stack.len().checked_sub(1)?;
        let top = &stack[depth];
        let idx = top.len().checked_sub(1)?;
        (top[idx] == h).then_some((depth, idx))
    })
}

/// The CURRENT handle in a slot from [`recorded_local_slot_of`] -- the
/// original address rewritten by every `update_local_refs_after_gc` since.
/// `None` (the caller keeps its own copy) unless the frame stack is back at
/// exactly that depth with the slot still present and non-null: a nested
/// native that leaked or over-popped a frame must not make this read someone
/// else's entry.
fn recorded_local_at(slot: (usize, usize)) -> Option<JObject> {
    JNI_LOCAL_FRAMES.with(|f| {
        let stack = f.try_borrow().ok()?;
        if stack.len() != slot.0 + 1 {
            return None;
        }
        stack[slot.0].get(slot.1).copied().filter(|&v| v != 0)
    })
}

/// Resolve a `JObject` to the underlying `ObjectRef`.
///
/// Two kinds of `JObject`:
///  - **Local ref** (bit 0 = 0): direct raw pointer to a heap object.
///  - **Global ref** (bit 0 = 1): tagged pointer to a `Box<ObjectRef>` in
///    `JniGlobalRefs`; dereference to get the actual ObjectRef.
///
/// For global refs, this function acquires the `jni_global_refs` Mutex via the
/// thread-local `JNI_SHARED_VM` to validate the handle is still alive before
/// dereferencing. This prevents a use-after-free race where a concurrent
/// `remove()` could deallocate the `Box` while we read it.
pub fn jobject_to_obj(jobj: JObject) -> Option<ObjectRef> {
    if jobj == 0 {
        None
    } else if is_jclass_handle(jobj) {
        // Tested before the bit-0 split: an odd `ClassId` would otherwise take
        // the global-ref branch and miss, an even one fail the heap check.
        jclass_handle_mirror(jobj)
    } else if jobj & 1 == 1 {
        // Global ref: resolve through the locked table to prevent
        // use-after-free if another thread concurrently calls remove().
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => {
                    let (obj, weak) = {
                        let refs = shared.natives.jni_global_refs.lock();
                        let obj = refs.resolve(jobj);
                        (obj, obj.is_some() && refs.is_weak(jobj))
                    };
                    // gc-common w8-c: resolving a WEAK global is a keep-alive,
                    // exactly as `Reference.get()` is
                    // (`NativeContextImpl::gc_reference_keep_alive`): a weak
                    // global is not a root, so a concurrent mark may not have
                    // reached its referent, and a native that stores what it
                    // resolved into an already-marked object would otherwise
                    // have it freed by that cycle. The SATB enqueue is inert
                    // (one load) when no cycle is marking. Outside the table
                    // lock: the barrier may flush into the collector's queue.
                    if weak {
                        if let Some(o) = obj {
                            shared.mem.heap.satb_barrier(Value::Object(Some(o)));
                        }
                    }
                    obj
                }
                None => {
                    tracing::warn!(
                        "jobject_to_obj: global ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    } else {
        // gc-common w6-g: an INDIRECT local handle (`CRATONVM_JNI_INDIRECT_LOCALS`)
        // names a slot of this thread's local frames; the address there is the
        // object's current one (the slot is a root and is remapped by every
        // moving collection). It then takes the same validation as a raw
        // handle. A deleted or popped slot is NULL.
        let jobj = match decode_indirect_local(jobj) {
            Some(slot) => read_local_slot(slot)?,
            None => jobj,
        };
        // Local ref: raw heap pointer.
        //
        // SECURITY FIX (V3): Previously this branch blindly reconstructed an
        // ObjectRef from the caller-supplied pointer with no validation, while
        // the global-ref branch above is protected by a locked table lookup. A
        // forged native pointer would become a wild read/write, and a local ref
        // held across a GC safepoint could be a stale from-space pointer under
        // the moving/generational collector. Mirror the global-ref path's
        // defensive posture: validate the address against the live heap (the
        // same `heap.is_heap_addr` check the GC root scanners use) and return
        // `None` on failure. `is_heap_addr` returns a freshly reconstructed
        // ObjectRef for a confirmed-live, aligned heap address, so we use its
        // result directly instead of an unchecked `from_raw`.
        //
        // The heap is reached through the same thread-local `JNI_SHARED_VM`
        // context the global-ref path uses (`SharedVm::heap`).
        //
        // FOLLOW-UP: the long-term fix is a full per-thread JNI local-handle
        // table (indirection handles validated on every access, like HotSpot's
        // JNIHandleBlock) so a local jobject can never be a raw heap pointer at
        // all. This address-validity gate is the minimum viable mitigation; it
        // does not catch a forged pointer that happens to land on a live
        // object, which only the handle table would fully prevent.
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => {
                    note_suspect_local_ref(shared, jobj as usize);
                    shared.mem.heap.is_heap_addr(jobj as usize)
                }
                None => {
                    tracing::warn!(
                        "jobject_to_obj: local ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    }
}

/// `CRATONVM_DBG=jni-localref` — report a LOCAL `jobject` naming an address a
/// recent collection moved an object away from.
///
/// # What this decides
///
/// A local ref is a raw heap pointer with no remap table (see the branch
/// above), so a handle a foreign `.so` holds across a moving collection comes
/// back naming from-space. When the vacated span has been re-issued, its
/// header reads back all-zero, `ClassId(0)` is `java/lang/Object`, and the
/// dispatch that follows raises `NoSuchMethodError java/lang/Object.<method>`.
///
/// The netty `ParameterizedSslHandlerTest` stall produces exactly that line
/// from at least three different producers, and one of them —
/// `checkClientTrusted` on the OPENSSL provider, which reaches the trust
/// manager through tcnative's JNI — has never had its holder named, because no
/// guard fires on its receiver. This one does, at the boundary, with the
/// backtrace that names the JNI entry point.
///
/// # What it cannot become
///
/// A stronger validity check here would NOT fix it: `is_heap_addr` is
/// alignment plus live-region containment, `is_object_address` adds header-tag
/// validation, and an all-zero header satisfies both (`kind` tag 0 is
/// `ObjectKind::Object`, `element_type` 0 is `Reference`). Only the per-thread
/// handle table this branch's FOLLOW-UP note describes removes the hazard.
///
/// Off by default and free when off: one relaxed load of the flag. Armed, it
/// costs a probe of the relocation ring, which itself only holds anything when
/// `CRATONVM_DBG=gcpart` is also set — so the two are meant to be armed
/// together, and the report says so when they are not.
#[inline]
fn note_suspect_local_ref(shared: &SharedVm, addr: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JNI_LOCALREF").is_some()
    });
    if !on {
        return;
    }
    static REPORTED: AtomicU64 = AtomicU64::new(0);
    const MAX_REPORTS: u64 = 24;
    let forwards = crate::memory::gc::gcpart_probe(addr);
    let moved: Vec<String> = forwards
        .iter()
        .filter_map(|(epoch, moved_to, _len, _dest)| {
            moved_to.map(|to| format!("epoch {epoch} -> {to:#x}"))
        })
        .collect();
    if moved.is_empty() {
        return;
    }
    if REPORTED.fetch_add(1, Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }
    let live_base = shared.mem.heap.is_object_address(addr).is_some();
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        forwards = moved.join(", "),
        passes_is_object_address = live_base,
        backtrace = %std::backtrace::Backtrace::force_capture(),
        "a JNI LOCAL ref names an address a recent collection moved an object \
         away from. A local ref is a raw heap pointer with no remap table, so \
         the foreign caller in the backtrace is holding a stale handle; the \
         next dispatch on it resolves ClassId(0) == java/lang/Object. Note \
         `passes_is_object_address`: an all-zero header satisfies that check, \
         so no validity gate here can catch this.",
    );
}

/// Old-style sentinel for use in JniLocalFrame (kept for compatibility).
pub struct JniLocalFrame {
    pub refs: Vec<ObjectRef>,
}

impl JniLocalFrame {
    pub fn new() -> Self {
        Self {
            refs: Vec::with_capacity(16),
        }
    }
    pub fn add(&mut self, obj: ObjectRef) -> JObject {
        self.refs.push(obj);
        obj_to_jobject(obj)
    }
    pub fn remove(&mut self, obj: JObject) {
        // SECURITY FIX (V3) follow-up: match the stored ref by its raw jobject
        // handle directly. The frame's own refs are already trusted, live
        // ObjectRefs, so removal must NOT route through the heap-validating
        // `jobject_to_obj` (which returns `None` outside an active JNI context
        // — e.g. in unit tests or any non-JNI caller — and would silently leak
        // the ref). `obj_to_jobject` is a pure ref→handle conversion.
        self.refs.retain(|r| obj_to_jobject(*r) != obj);
    }
}

impl Default for JniLocalFrame {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum length for C strings received from native code.
const MAX_JNI_CSTR_LEN: usize = 65536;

/// Convert a C string pointer to a Rust `&str`, with length bounds checking.
///
/// # Safety
/// `ptr` must point to a valid null-terminated C string (or be null).
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // Scan up to MAX_JNI_CSTR_LEN bytes to avoid unbounded reads on malformed input.
    let mut len = 0;
    while len < MAX_JNI_CSTR_LEN {
        if *ptr.add(len) == 0 {
            let slice = std::slice::from_raw_parts(ptr as *const u8, len);
            return std::str::from_utf8(slice).ok();
        }
        len += 1;
    }
    tracing::warn!("JNI cstr_to_str: string exceeds {MAX_JNI_CSTR_LEN} byte limit");
    None
}

/// Encode a Rust `&str` into JNI "modified UTF-8" (JNI spec, "Modified UTF-8
/// Strings"). This differs from standard UTF-8 in two ways:
///
///   * `U+0000` is encoded as the two bytes `0xC0 0x80` (never a single `0x00`),
///     so the byte stream contains no interior NUL and can be terminated by a
///     single trailing `0x00` like a C string. Returning null for strings that
///     contain an embedded NUL — as `CString::new` would force — violates the
///     JNI contract for `GetStringUTFChars`.
///   * Supplementary characters (code points `> U+FFFF`) are encoded as a UTF-16
///     surrogate pair, each surrogate then emitted as its own three-byte
///     sequence (CESU-8). A modified-UTF-8 sequence therefore never exceeds
///     three bytes per unit, matching HotSpot's behaviour.
///
/// The returned `Vec<u8>` contains no `0x00` bytes, so it can be wrapped in a
/// `CString` without error.
fn to_modified_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 1);
    for ch in s.chars() {
        let cp = ch as u32;
        match cp {
            // U+0000 — encode as the overlong two-byte form, not a NUL byte.
            0x0000 => out.extend_from_slice(&[0xC0, 0x80]),
            // U+0001..U+007F — single byte (ASCII), unchanged.
            0x0001..=0x007F => out.push(cp as u8),
            // U+0080..U+07FF — two bytes.
            0x0080..=0x07FF => {
                out.push(0xC0 | (cp >> 6) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
            // U+0800..U+FFFF — three bytes (BMP, including the non-surrogate
            // range; `char` never holds a lone surrogate, so this is safe).
            0x0800..=0xFFFF => {
                out.push(0xE0 | (cp >> 12) as u8);
                out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
            // Supplementary plane — emit a UTF-16 surrogate pair, each surrogate
            // as a three-byte sequence (CESU-8), per the JNI spec.
            _ => {
                let v = cp - 0x1_0000;
                let hi = 0xD800 | (v >> 10);
                let lo = 0xDC00 | (v & 0x3FF);
                for unit in [hi, lo] {
                    out.push(0xE0 | (unit >> 12) as u8);
                    out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                    out.push(0x80 | (unit & 0x3F) as u8);
                }
            }
        }
    }
    out
}

/// Number of bytes [`to_modified_utf8`] would produce for `s`, without
/// allocating the encoded buffer. Used by `GetStringUTFLength` so the reported
/// length agrees with the bytes `GetStringUTFChars` returns.
fn modified_utf8_len(s: &str) -> usize {
    let mut n = 0usize;
    for ch in s.chars() {
        let cp = ch as u32;
        n += match cp {
            0x0000 => 2,          // overlong NUL
            0x0001..=0x007F => 1, // ASCII
            0x0080..=0x07FF => 2, // two-byte
            0x0800..=0xFFFF => 3, // three-byte BMP
            _ => 6,               // surrogate pair: two 3-byte sequences
        };
    }
    n
}

/// Encode a method identity as a JMethodID.
/// We pack (class_id, method_index) into a u64.
fn encode_method_id(class_id: ClassId, method_index: u16) -> JMethodID {
    ((class_id.as_u32() as u64) << 32) | (method_index as u64)
}

/// Decode a JMethodID back to (class_id, method_index).
fn decode_method_id(mid: JMethodID) -> (ClassId, u16) {
    let class_id = ClassId::new((mid >> 32) as u32);
    let method_index = (mid & 0xFFFF) as u16;
    (class_id, method_index)
}

/// Encode a field identity as a JFieldID.
/// We pack (class_id, field_index) into a u64.
fn encode_field_id(class_id: ClassId, field_index: usize) -> JFieldID {
    ((class_id.as_u32() as u64) << 32) | (field_index as u64)
}

/// Decode a JFieldID back to (class_id, field_index).
fn decode_field_id(fid: JFieldID) -> (ClassId, usize) {
    let class_id = ClassId::new((fid >> 32) as u32);
    let field_index = (fid & 0xFFFF_FFFF) as usize;
    (class_id, field_index)
}

// ---------------------------------------------------------------------------
// JNI function implementations (extern "C")
// ---------------------------------------------------------------------------

// ---- Index 4: GetVersion ----
extern "C" fn jni_get_version(_env: JNIEnv) -> JInt {
    JNI_VERSION_1_8
}

// ---- Index 6: FindClass ----
//
// Round 12 wave 7 (lane compat), both modes
// (`r12w6-jni-findclass-ignores-the-native-loader-and-raises-nothing-20260927.md`):
//
// * the class is resolved as the calling Java code would resolve it
//   ([`jni_find_class_in_caller_context`]), so a native of a class a
//   `URLClassLoader` defined finds its siblings; a name in dotted form is not a
//   class name and is refused, as HotSpot refuses it. Kill switch
//   `CRATONVM_JNI_FINDCLASS_LOADER=0` (the application-namespace lookup below,
//   for every caller, dotted names included);
// * a failure leaves `NoClassDefFoundError: <name>` (or the typed linkage
//   error) pending, as HotSpot's `resolve_or_fail` does, instead of a bare
//   NULL. Kill switch `CRATONVM_JNI_FINDCLASS_RAISES=0`.
extern "C" fn jni_find_class(_env: JNIEnv, name: *const c_char) -> JClass {
    let _fx = ForeignJniEntry::enter();
    let raises = jni_find_class_raises();
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => {
            if raises {
                // HotSpot's `SystemDictionary::class_name_symbol` text for a
                // NULL name; an undecodable one names nothing it could find.
                raise_jni_no_class_def_found(if name.is_null() {
                    "No class name given"
                } else {
                    "<class name is not valid UTF-8>"
                });
            }
            return 0;
        }
    };
    if jni_find_class_loader_aware() {
        // Internal form only: no class name contains `.` (JVMS 4.2.1), so
        // `java.lang.String` names no class -- HotSpot's loader answers
        // `java/lang/String`, the name check refuses it, and FindClass fails.
        if name_str.contains('.') {
            if raises {
                raise_jni_no_class_def_found(name_str);
            }
            return 0;
        }
        match jni_find_class_in_caller_context(name_str, raises) {
            JniFindClassOutcome::Found(cid) => {
                if jni_find_class_initialize(cid, name_str, raises) {
                    return class_id_to_jclass(cid);
                }
                if raises && JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                    raise_jni_no_class_def_found(name_str);
                }
                return 0;
            }
            JniFindClassOutcome::Failed => {
                // Never a bare NULL: whatever the resolution could not type
                // still leaves `NoClassDefFoundError` pending.
                if raises && JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                    raise_jni_no_class_def_found(name_str);
                }
                return 0;
            }
            JniFindClassOutcome::NoCallerContext => {}
        }
    }
    let found = with_shared_vm(|shared| shared.load_class_concurrent(name_str).ok()).flatten();
    match found {
        Some(class_id) => {
            if jni_find_class_initialize(class_id, name_str, raises) {
                return class_id_to_jclass(class_id);
            }
            if raises && JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                raise_jni_no_class_def_found(name_str);
            }
            0
        }
        None => {
            if raises {
                raise_jni_no_class_def_found(name_str);
            }
            0
        }
    }
}

/// JNI `FindClass` hands back an INITIALIZED class, as HotSpot's
/// `jni_FindClass` does (`find_class_from_class_loader(.., init = true, ..)`):
/// a static read through `GetStaticFieldID` / `GetStatic<T>Field` -- which read
/// the statics table directly and initialize nothing -- then sees `<clinit>`'s
/// values, and a failing `<clinit>` surfaces from `FindClass` as its
/// `ExceptionInInitializerError` / `NoClassDefFoundError`. `true` when the
/// class is (or needs nothing to be) initialized; `false` when initialization
/// threw, which is left pending when `raises`. An array class has no
/// `<clinit>`; a thread with no JNI context cannot run Java, and keeps the
/// uninitialized answer it always had. Round 12 wave 8 (lane compat2), both
/// modes; kill switch `CRATONVM_JNI_FINDCLASS_INITIALIZES=0`.
fn jni_find_class_initialize(class_id: ClassId, name: &str, raises: bool) -> bool {
    if name.starts_with('[') || !jni_find_class_initializes() {
        return true;
    }
    // `<clinit>` is Java: a foreign-attached thread must be a counted mutator
    // while it runs, as for the loader call in the caller-context lookup.
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        match crate::vm::ensure_class_initialized_shared(shared, thread, class_id) {
            Ok(()) => true,
            Err(err) => {
                if raises {
                    let _ = jni_surface_jdk_only(shared, thread, Err(err));
                }
                false
            }
        }
    })
    .unwrap_or(true)
}

/// `CRATONVM_JNI_FINDCLASS_INITIALIZES` -- default ON (round 12 wave 8, lane
/// compat2, both modes). `0` restores the uninitialized `FindClass` answer.
/// Read per call, as [`jni_find_class_loader_aware`].
fn jni_find_class_initializes() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JNI_FINDCLASS_INITIALIZES")
}

/// What [`jni_find_class_in_caller_context`] came back with.
enum JniFindClassOutcome {
    Found(ClassId),
    /// Resolution failed; what it threw is pending when `raises` asked for it.
    Failed,
    /// No attached `JvmThread`, or no Java frame on it (a host thread that
    /// attached and called FindClass directly): the application-namespace
    /// lookup answers, which is HotSpot's system-loader answer.
    NoCallerContext,
}

/// JNI `FindClass`'s loader, as HotSpot's `jni_FindClass` picks it: the
/// loader of the Java code that called the native -- CratonVM pushes no frame
/// for the native itself, so the innermost Java frame (compiled frames
/// included) is its caller, which is the native's own class or a class of the
/// same loader in the ordinary case -- except from `JNI_OnLoad` /
/// `JNI_OnUnload`, where that frame is the JDK's `NativeLibraries` machinery
/// and the context is `NativeLibraries.getFromClass()`, the class that called
/// `System.loadLibrary`.
///
/// `name` is resolved exactly as a constant-pool class reference of that
/// class resolves ([`resolve_class_loader_aware`]): a built-in context takes
/// the same global lookup the legacy path takes, a user-defined loader is
/// asked first. A failure is converted as an opcode's is
/// (`convert_class_not_found_for`: `NoClassDefFoundError: <name>` caused by
/// the `ClassNotFoundException`) and published pending when `raises`.
///
/// [`resolve_class_loader_aware`]: crate::runtime::interpreter::resolve_class_loader_aware
fn jni_find_class_in_caller_context(name: &str, raises: bool) -> JniFindClassOutcome {
    // The loader's `loadClass` is Java: a foreign-attached thread must be a
    // counted mutator while it runs.
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let Some(context) = jni_find_class_context(shared, thread) else {
            return JniFindClassOutcome::NoCallerContext;
        };
        match crate::runtime::interpreter::resolve_class_loader_aware(shared, thread, context, name)
        {
            Ok(cid) => JniFindClassOutcome::Found(cid),
            Err(err) => {
                if raises {
                    let failed = crate::runtime::exceptions::convert_class_not_found_for(
                        shared,
                        thread,
                        Some(context),
                        name,
                        err,
                    );
                    let _ = jni_surface_jdk_only(shared, thread, Err(failed));
                }
                JniFindClassOutcome::Failed
            }
        }
    })
    .unwrap_or(JniFindClassOutcome::NoCallerContext)
}

/// The class whose loader [`jni_find_class_in_caller_context`] resolves
/// through, or `None` when the thread has no Java frame.
fn jni_find_class_context(shared: &SharedVm, thread: &mut JvmThread) -> Option<ClassId> {
    // The native being dispatched, not its caller: HotSpot's
    // `security_get_caller_class(0)` sees the native frame, CratonVM pushes
    // none, so `vm_exec.rs`'s `JniContextGuard::install_for_native` records
    // the native's declaring class on the thread
    // (`r12w8-compat2-jni-findclass-native-declaring-class-patch`).
    let native_holder = if jni_find_class_native_holder() {
        thread.jni_native_class
    } else {
        None
    };
    let innermost = crate::runtime::stackwalker::frame_class_ids_with_compiled(&thread.frames)
        .first()
        .copied()
        .or(native_holder)?;
    const NATIVE_LIBRARIES: &str = "jdk/internal/loader/NativeLibraries";
    let native_libraries = {
        let cm = shared.classes.class_manager.read();
        let from_library_loader = cm.get_class(innermost).is_some_and(|c| {
            matches!(c.loader_id, cratonvm_types::ClassLoaderId::Bootstrap)
                && (&*c.name == NATIVE_LIBRARIES
                    || c.name
                        .strip_prefix(NATIVE_LIBRARIES)
                        .is_some_and(|rest| rest.starts_with('$')))
        });
        if from_library_loader {
            cm.get_loaded_class_id(NATIVE_LIBRARIES)
        } else {
            None
        }
    };
    let Some(native_libraries) = native_libraries else {
        return Some(native_holder.unwrap_or(innermost));
    };
    // `JNI_OnLoad` / `JNI_OnUnload`: HotSpot asks the library machinery which
    // class is loading (`getFromClass`, "invoked in the VM to determine the
    // context class"); it answers `Object.class` outside a load.
    match crate::vm::invoke_on_class_shared(
        shared,
        thread,
        native_libraries,
        "getFromClass",
        "()Ljava/lang/Class;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(mirror)))) => {
            Some(crate::vm::class_id_from_mirror(shared, mirror).unwrap_or(innermost))
        }
        _ => Some(innermost),
    }
}

/// `CRATONVM_JNI_FINDCLASS_LOADER` -- default ON (round 12 wave 7, lane
/// compat, both modes). `0` / `false` / `off` / `no` restores the
/// application-namespace lookup for every caller. Read per call, not cached:
/// a declared flag is read from the immutable flag snapshot, and a cached
/// switch would be one more `static` under
/// `vm/tests/per_vm_state_statics_ratchet.rs`.
fn jni_find_class_loader_aware() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JNI_FINDCLASS_LOADER")
}

/// `CRATONVM_JNI_FINDCLASS_RAISES` -- default ON (round 12 wave 7, lane
/// compat, both modes). `0` restores the bare NULL with nothing pending.
/// Read per call, as [`jni_find_class_loader_aware`].
fn jni_find_class_raises() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JNI_FINDCLASS_RAISES")
}

/// `CRATONVM_JNI_FINDCLASS_NATIVE_HOLDER` -- default ON (round 12 wave 8,
/// both modes). `0` restores the innermost-Java-frame (the caller's) loader
/// for a JNI native's `FindClass`. Read per call, as
/// [`jni_find_class_loader_aware`].
fn jni_find_class_native_holder() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JNI_FINDCLASS_NATIVE_HOLDER")
}

// ---- Index 10: GetSuperclass ----
extern "C" fn jni_get_superclass(_env: JNIEnv, clazz: JClass) -> JClass {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        // JNI: "If clazz specifies ... an interface, returns NULL". The class
        // file of an interface names `java/lang/Object` as its super_class,
        // and the class store keeps it, so this answered `Object`
        // (`Class.getSuperclass` answers null, `native_class_get_superclass`;
        // round 12 wave 6, lane jni). Both modes: the owner turned it on for
        // `--compatible` too (2026-09-27).
        if class.is_interface() {
            return None;
        }
        class.superclass.map(class_id_to_jclass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 11: IsAssignableFrom ----
extern "C" fn jni_is_assignable_from(_env: JNIEnv, sub: JClass, sup: JClass) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    if sub == 0 || sup == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let sub_id = jclass_class_id(sub);
        let sup_id = jclass_class_id(sup);
        if sub_id == sup_id {
            return JNI_TRUE;
        }
        let walked = {
            let cm = shared.classes.class_manager.read();
            let mut current = sub_id;
            loop {
                let Some(class) = cm.get_class(current) else {
                    return JNI_FALSE;
                };
                // Check interfaces
                if class.interfaces.contains(&sup_id) {
                    break true;
                }
                match class.superclass {
                    Some(sc) => {
                        if sc == sup_id {
                            break true;
                        }
                        current = sc;
                    }
                    None => break false,
                }
            }
        };
        // The walk sees only DIRECT interfaces; see `jni_is_instance_of`.
        if walked || crate::runtime::interpreter::class_is_subtype(shared, sub_id, sup_id) {
            return JNI_TRUE;
        }
        // Round 11 wave 15 (lane rt, `r11w13-rt-array-alias-audit-jni-refs-gc`
        // open item 2): an ARRAY class's own entries name only `Object`,
        // `Cloneable` and `Serializable`, so covariance -- `String[]` to
        // `Object[]`, `Integer[][]` to `Number[][]` -- is invisible to both
        // relations above, and this answered `JNI_FALSE` where HotSpot answers
        // `JNI_TRUE`. Ask the `instanceof` opcode's strict array rule, exactly
        // as `jni_is_instance_of` does for an array object; an array class's
        // name IS its descriptor.
        let names = {
            let cm = shared.classes.class_manager.read();
            match (cm.get_class(sub_id), cm.get_class(sup_id)) {
                (Some(sub), Some(sup)) => Some((sub.name.to_string(), sup.name.to_string())),
                _ => None,
            }
        };
        match names {
            Some((sub_name, sup_name))
                if sub_name.starts_with('[')
                    && crate::runtime::interpreter::array_is_instance_of(
                        shared, &sub_name, &sup_name,
                    ) =>
            {
                JNI_TRUE
            }
            _ => JNI_FALSE,
        }
    })
    .unwrap_or(JNI_FALSE)
}

// ---- Index 13: Throw ----
extern "C" fn jni_throw(_env: JNIEnv, obj: JThrowable) -> JInt {
    let _fx = ForeignJniEntry::enter();
    // `Throw(env, cls)` (meant: `ThrowNew`) is a common native bug. A `jclass`
    // now resolves to its mirror, which is not a `Throwable`; keep refusing it
    // as before rather than making a `Class` the pending exception.
    if is_jclass_handle(obj) {
        return JNI_ERR;
    }
    let Some(exception) = jobject_to_obj(obj) else {
        return JNI_ERR;
    };
    set_jni_pending_exception_object(exception);
    JNI_OK
}

// ---- Index 14: ThrowNew ----
extern "C" fn jni_throw_new(_env: JNIEnv, clazz: JClass, msg: *const c_char) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return JNI_ERR;
    }
    let message = if msg.is_null() {
        None
    } else {
        match unsafe { cstr_to_str(msg) } {
            Some(message) => Some(message),
            None => return JNI_ERR,
        }
    };

    let result = with_jni_context(|shared, thread| {
        let class_id = jclass_class_id(clazz);
        let class_name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(class_id).map(|class| class.name.to_string())
        };
        let Some(class_name) = class_name else {
            return None;
        };
        crate::runtime::exceptions::create_exception_object_for_jni_throw_new(
            shared,
            thread,
            class_id,
            &class_name,
            message,
        )
        .ok()
    });

    match result.flatten() {
        Some(exception) => {
            set_jni_pending_exception_object(exception);
            JNI_OK
        }
        None => JNI_ERR,
    }
}

// ---- Index 15: ExceptionOccurred ----
extern "C" fn jni_exception_occurred(_env: JNIEnv) -> JThrowable {
    let _fx = ForeignJniEntry::enter();
    let handle = JNI_PENDING_EXCEPTION.with(|cell| cell.get());
    if handle == 0 {
        return 0;
    }
    // A collection may have relocated the exception after its original raw
    // handle was recorded.  Prefer the VM's remapped native-return root.
    //
    // Handed out as a RECORDED local ref (gc-common w4-g): the idiom is
    // `t = ExceptionOccurred(); ExceptionClear(); ... use t`, and
    // `ExceptionClear` drops `native_pending_return` -- the only root that
    // named the throwable -- so the native's `t` was held by nothing across
    // whatever up-call came next. HotSpot returns a local ref here.
    //
    // gcd d4/k (`docs/internal/gc/gcd-d3k-jni-exception-sentinel-escapes-to-native-FIXED-20260928.md`):
    // the `u64::MAX` sentinel ("no throwable could be built") is not a handle
    // and is never handed to native code; NULL instead (ExceptionCheck still
    // answers true, and the native's return still fails). A heap failure now
    // pends the preallocated OOME (`pend_unbuilt_throwable`), so this only
    // remains for a class that could not load or no thread context.
    let fallback = if handle == u64::MAX { 0 } else { handle };
    with_jni_context(|_shared, thread| match thread.native_pending_return {
        Some(exc) => new_local_handle(exc),
        None => {
            // The raw pending handle, not a recorded local: counted as an
            // escape where locals are indirect, so an in-native call stays
            // counted from here on (`JniNativeCall`).
            if fallback != 0 && locals_are_indirect_here() {
                note_raw_local_escape();
            }
            fallback
        }
    })
    .unwrap_or(fallback)
}

// ---- Index 16: ExceptionDescribe ----
///
/// Prints the pending exception and **clears it** — the clear is the part that
/// matters for control flow, and the empty no-op this used to be did neither.
/// `if (ExceptionCheck(env)) { ExceptionDescribe(env); return -1; }` is the
/// canonical JNI error path, and it is written on the understanding that
/// nothing is pending afterwards; leaving the exception in place means it is
/// delivered at the native's return to a caller that believes it was handled.
extern "C" fn jni_exception_describe(_env: JNIEnv) {
    let _fx = ForeignJniEntry::enter();
    let handle = JNI_PENDING_EXCEPTION.with(|cell| cell.get());
    if handle == 0 {
        return;
    }
    // Prefer the GC-remapped root over the raw handle, as `ExceptionOccurred`
    // does — a collection may have moved the throwable since it was recorded.
    // gcd d4/k: the `u64::MAX` sentinel names no object; not decoded (it used
    // to take the global-ref path, miss and log).
    let exc = with_jni_context(|_shared, thread| thread.native_pending_return)
        .flatten()
        .or_else(|| {
            (handle != u64::MAX)
                .then(|| jobject_to_obj(handle))
                .flatten()
        });
    // Clear FIRST: the print below is a Java up-call, and it must not run with
    // this exception still pending.
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
    let _ = with_jni_context(|_shared, thread| {
        thread.native_pending_return = None;
    });
    let Some(exc) = exc else {
        return;
    };
    let _ = with_jni_context(|shared, thread| {
        let class_id = shared.mem.heap.class_id_of(exc);
        let printed = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            "printStackTrace",
            "()V",
            &[Value::Object(Some(exc))],
        );
        // A failure while REPORTING must not become a second pending
        // exception the caller never asked about.
        if printed.is_err() {
            JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
            thread.native_pending_return = None;
        }
    });
}

// ---- Index 17: ExceptionClear ----
extern "C" fn jni_exception_clear(_env: JNIEnv) {
    let _fx = ForeignJniEntry::enter();
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
    let _ = with_jni_context(|_shared, thread| {
        thread.native_pending_return = None;
    });
}

// ---- Index 18: FatalError ----
extern "C" fn jni_fatal_error(_env: JNIEnv, msg: *const c_char) {
    let s = unsafe { cstr_to_str(msg).unwrap_or("unknown") };
    eprintln!("JNI FatalError: {s}");
    std::process::abort();
}

// ---- Index 20: PushLocalFrame ----
///
/// gc-common w10-c: a negative `capacity` is refused with `OutOfMemoryError`
/// pending and `JNI_ERR`, as HotSpot does (`jni_PushLocalFrame`), and pushes
/// nothing; it used to push a frame and answer `JNI_OK`. A large one is only
/// a reservation hint ([`push_local_frame`]).
extern "C" fn jni_push_local_frame(_env: JNIEnv, capacity: JInt) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if capacity < 0 {
        refuse_local_capacity("PushLocalFrame", capacity);
        return JNI_ERR;
    }
    let depth = local_frame_depth();
    push_local_frame(capacity as usize);
    JNI_EXPLICIT_FRAME_DEPTHS.with(|d| d.borrow_mut().push(depth));
    JNI_OK
}

// ---- Index 21: PopLocalFrame ----
///
/// gc-common w7-c: pops only a frame native code PUSHED. An unbalanced
/// `PopLocalFrame` (more pops than pushes, which the spec forbids) used to pop
/// whatever was on top: the native's own implicit dispatch frame, and on the
/// next extra pop the CALLER's -- an outer native's frame, whose locals that
/// native still holds, unrooted under it and no longer remapped by a moving
/// collection. The scope's own exit then popped one frame more. HotSpot
/// ignores such a pop and returns `result` unchanged
/// (`jni_PopLocalFrame`: "code will still work if PopLocalFrame is called
/// without a corresponding PushLocalFrame"); so does this now.
extern "C" fn jni_pop_local_frame(_env: JNIEnv, result: JObject) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if !top_local_frame_is_explicit() {
        return result;
    }
    pop_local_frame(result)
}

/// Tag bit that lifts a `ClassId` out of the range where `0` means JNI NULL.
///
/// **The defect this closes.** A `jclass` in this table IS a `ClassId` — that is
/// what `FindClass` returns and what `GetSuperclass`, `GetFieldID`,
/// `RegisterNatives` and eighteen others decode with `ClassId::new(clazz as
/// u32)`. But `jclass` is a `jobject`, and in JNI a NULL `jobject` means
/// *failure*. So whichever class happened to be `ClassId(0)` was unreachable
/// through `FindClass`: the call returned 0 and every caller read it as "no such
/// class".
///
/// `java.lang.Object` is the first class this VM loads, so `ClassId(0)` is
/// `java.lang.Object` — the one class essentially every JNI library asks for
/// first. Measured with a C probe calling `FindClass` from both `JNI_OnLoad` and
/// a registered native: `FindClass(java/lang/Object) = (nil)` on both, against
/// a live handle on HotSpot.
///
/// Downstream that surfaced as nothing resembling a JNI defect. JNA's
/// `libjnidispatch` aborts its id cache with `JNA: Problems loading core IDs:
/// java.lang.Object`, then never caches `classString`/`MID_String_init`, so
/// every `jstring` it later builds is NULL — and `com.sun.jna.Native.<clinit>`
/// dies on `NullPointerException: Cannot invoke "String.split(String)" because
/// "nativeVersion" is null`, which reads like a JNA/version problem.
///
/// **Why a HIGH tag and not a +1 bias.** The tag lives entirely above bit 31, so
/// the low 32 bits stay exactly the `ClassId` and all twenty-one existing
/// `ClassId::new(clazz as u32)` decode sites keep working untouched — the
/// truncation discards the tag. A bias would have needed every one of them
/// changed in lockstep, and a single missed site would silently decode the WRONG
/// class rather than fail.
///
/// **Why these particular bits.** Bits 48-62 are set, which no x86-64 or AArch64
/// user-space pointer can have (canonical user addresses are below
/// `0x0000_8000_0000_0000`). So a tagged `jclass` cannot be mistaken for either
/// of the two object-handle encodings — a raw heap pointer or a tagged
/// `Box<ObjectRef>` — and [`is_jclass_handle`] is an exact test rather than a
/// guess that has to be tried second.
const JCLASS_TAG: JObject = 0x7F51_0000_0000_0000;

/// Mask selecting the tag half of a `jclass` handle.
const JCLASS_TAG_MASK: JObject = 0xFFFF_FFFF_0000_0000;

/// Encode a `ClassId` as the `jclass` handle native code receives.
pub fn class_id_to_jclass(id: ClassId) -> JClass {
    (id.as_u32() as JObject) | JCLASS_TAG
}

/// Does this handle carry the `jclass` encoding at all? Cheap, allocation-free
/// and lock-free — no class-manager lookup, so it is safe to consult FIRST, in
/// paths that would otherwise log a spurious "not found in global ref table".
fn is_jclass_handle(h: JObject) -> bool {
    h & JCLASS_TAG_MASK == JCLASS_TAG
}

/// The `java.lang.Class` mirror a `jclass` handle names, for every place a
/// `jclass` crosses back into Java as an OBJECT: a native's `Class` return
/// (`dispatch_jni_native`), `SetObjectField` / `SetStaticObjectField` /
/// `SetObjectArrayElement` / `NewObjectArray`'s initial element, an up-call
/// argument, `MonitorEnter(cls)`, `GetObjectClass(cls)`. On HotSpot a `jclass`
/// IS a reference to the mirror; here it is a tagged `ClassId`
/// ([`class_id_to_jclass`]), and [`jobject_to_obj`] had no arm for it, so all of
/// these read Java `null` (round 12 wave 1, lane rt;
/// `w36-jni-native-returning-a-jclass-hands-java-null`).
///
/// `None` for a class this VM does not have (a forged or stale tag), and when
/// the mirror does not exist yet and the heap cannot hold it without a
/// collection: the mint is [`crate::vm::try_get_or_create_class_mirror`], which
/// never collects, because every caller holds raw `ObjectRef`s resolved before
/// it. No lock is held across the two lookups, so the lock order is the one
/// `class_mirror_impl` itself uses. Kill switch:
/// `CRATONVM_JNI_JCLASS_MIRROR=0` (the old `null`).
fn jclass_handle_mirror(h: JObject) -> Option<ObjectRef> {
    if !jclass_mirror_decode_active() {
        return None;
    }
    let class_id = ClassId::new(h as u32);
    with_shared_vm(|shared| {
        let cached = shared.classes.class_mirrors.read().get(&class_id).copied();
        if cached.is_some() {
            return cached;
        }
        let known = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some();
        if !known {
            return None;
        }
        crate::vm::try_get_or_create_class_mirror(shared, class_id)
    })
    .flatten()
}

/// `CRATONVM_JNI_JCLASS_MIRROR`, latched like every flag. Default ON; `0` /
/// `false` / `off` / `no` restores the pre-round-12 decode (a `jclass` read as
/// an object is `null`).
fn jclass_mirror_decode_active() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JNI_JCLASS_MIRROR")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("0" | "false" | "off" | "no")
        )
    })
}

/// Is this handle a `jclass` (as produced by [`class_id_to_jclass`]) naming a
/// live class?
///
/// A confirmed class is its own permanent reference: classes are never unloaded
/// here, so "a global ref to a class" is the same handle again. That identity is
/// what makes the round trip work, because native code stores the RESULT of
/// `NewGlobalRef`/`NewWeakGlobalRef` and later passes it back as a `jclass` to
/// `GetMethodID`/`NewObject`.
fn jclass_id_handle(h: JObject) -> bool {
    if !is_jclass_handle(h) {
        return false;
    }
    with_shared_vm(|shared| {
        shared
            .classes
            .class_manager
            .read()
            .get_class(ClassId::new(h as u32))
            .is_some()
    })
    .unwrap_or(false)
}

/// Normalise ANY handle that names a class into its `ClassId`.
///
/// A class reaches native code through two encodings, and until this helper
/// existed only one of them was decodable:
///
/// * `FindClass`, `GetObjectClass`, `GetSuperclass` and the `jclass` argument
///   of a static native all hand back [`class_id_to_jclass`] — a `ClassId` under
///   [`JCLASS_TAG`]. Every `jclass`-consuming entry point decodes that with
///   `ClassId::new(h as u32)`, which works because the truncation drops the tag;
/// * a `java.lang.Class` that arrives as an ordinary **parameter** — the native
///   is declared `foo(int, Class<?>, Object)` — is marshalled like any other
///   reference, as `obj_to_jobject` of its mirror. Truncating THAT to a `u32`
///   yields a fragment of a heap address, i.e. an arbitrary unrelated class or
///   no class at all.
///
/// Nothing reconciled the two, so a `Class` parameter was unusable: measured on
/// JDK 25 against a purpose-built JNI fixture, `IsSameObject(param, FindClass)`,
/// `GetMethodID`, `GetStaticFieldID`, `IsAssignableFrom` and `IsInstanceOf` all
/// answered "no"/NULL where HotSpot answered yes. The library that surfaced it
/// is barchart-udt, whose `SocketUDT.setOption0(int code, Class<?> klaz, Object
/// value)` dispatches on `IsSameObject(klaz, <cached Boolean.class>)` and
/// therefore rejected EVERY option — netty's `NioUdtProvider` could not open a
/// channel at all ("unsupported option class in OptionUDT").
///
/// The fix is at consumption rather than at marshalling on purpose: re-encoding
/// `Class` arguments as `jclass` handles would fix the reads and break the
/// writes, because a native that hands the same reference back to Java (or to
/// `GetObjectClass`) needs the mirror `ObjectRef`, which a `jclass` handle does
/// not carry. Reconciling here leaves both encodings intact and makes both
/// decodable.
///
/// Falls back to today's truncation for anything that is not a class, so no
/// existing decode changes: a handle that is neither a `jclass` nor a mirror
/// produced a truncated id before and produces the same one now. (The C JVMTI
/// table decodes through [`jclass_class_id_exact`], which has no fallback.)
pub(crate) fn jclass_class_id(h: JObject) -> ClassId {
    if is_jclass_handle(h) {
        return ClassId::new(h as u32);
    }
    if let Some(id) = jclass_mirror_class_id(h) {
        return id;
    }
    ClassId::new(h as u32)
}

/// [`jclass_class_id`] without its truncating fallback: `None` for a handle
/// that is neither a `jclass` nor a `java.lang.Class` mirror (a primitive
/// mirror included). The C JVMTI table's `jclass` decode
/// (`jvmti::native_env`, `JVMTI_ERROR_INVALID_CLASS`; interpreter round i1
/// wave 19): through the fallback, any other handle whose low 32 bits equal a
/// loaded class's id — an indirect local's are a small slot number — named
/// that class.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn jclass_class_id_exact(h: JObject) -> Option<ClassId> {
    if is_jclass_handle(h) {
        return Some(ClassId::new(h as u32));
    }
    jclass_mirror_class_id(h)
}

/// The `ClassId` a handle names IF it is a `java.lang.Class` mirror reference.
/// `None` for a `jclass` handle (already decodable), for a non-class object,
/// and for a primitive/void mirror, which has no `ClassId`.
fn jclass_mirror_class_id(h: JObject) -> Option<ClassId> {
    if h == 0 || is_jclass_handle(h) {
        return None;
    }
    let oref = jobject_to_obj(h)?;
    with_shared_vm(|shared| crate::vm::class_id_from_mirror(shared, oref)).flatten()
}

/// The identity a handle has for `IsSameObject` purposes: a class compares by
/// `ClassId` regardless of which of the two encodings it arrived in.
enum JniIdentity {
    Class(ClassId),
    Object(ObjectRef),
    Null,
}

fn jni_identity(h: JObject) -> JniIdentity {
    if h == 0 {
        return JniIdentity::Null;
    }
    if is_jclass_handle(h) {
        return JniIdentity::Class(ClassId::new(h as u32));
    }
    if let Some(id) = jclass_mirror_class_id(h) {
        return JniIdentity::Class(id);
    }
    match jobject_to_obj(h) {
        Some(r) => JniIdentity::Object(r),
        None => JniIdentity::Null,
    }
}

// ---- Index 22: NewGlobalRef ----
extern "C" fn jni_new_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0;
    }
    // A `jclass` is checked FIRST: it is an exact, lock-free bit test
    // (`is_jclass_handle`), and routing it through `jobject_to_obj` would log a
    // spurious "handle … not found in global ref table" for every odd ClassId.
    if jclass_id_handle(obj) {
        return obj;
    }
    // Resolve the object whether it's a local ref or another global ref.
    let oref = match jobject_to_obj(obj) {
        Some(r) => r,
        None => {
            // `NewGlobalRef(FindClass(env, "..."))` is the first thing almost
            // every `JNI_OnLoad` does, and a `jclass` here is a raw `ClassId`,
            // not an object handle — so this returned 0 and the library
            // concluded the JVM had no `java.lang.Object`. Measured on JNA
            // 5.13.0: `libjnidispatch`'s init prints `JNA: Problems loading
            // core IDs: java.lang.Object`, gives up caching `classString` /
            // `MID_String_init`, and every later `jstring` it builds is NULL —
            // surfacing as `Native.<clinit>` throwing
            // `NullPointerException: Cannot invoke "String.split(String)"
            // because "nativeVersion" is null`, which reads like a missing JNA
            // feature and is in fact a dead JNI primitive.
            return 0;
        }
    };
    with_shared_vm(|shared| {
        let handle = shared.natives.jni_global_refs.lock().add(oref);
        Some(handle)
    })
    .flatten()
    .unwrap_or_else(|| {
        tracing::warn!("JNI NewGlobalRef: failed to create global reference for handle {obj:#x}");
        0
    })
}

// ---- Index 23: DeleteGlobalRef ----
extern "C" fn jni_delete_global_ref(_env: JNIEnv, gref: JObject) {
    let _fx = ForeignJniEntry::enter();
    if gref == 0 || gref & 1 == 0 {
        return; // not a global ref handle
    }
    // A `jclass` handed back from `NewGlobalRef` above can have bit 0 set (it is
    // the low bit of the ClassId). Deleting it must be a no-op — the class
    // outlives every ref to it — and must not disturb the ref table.
    if is_jclass_handle(gref) {
        return;
    }
    with_shared_vm(|shared| {
        shared.natives.jni_global_refs.lock().remove(gref);
    });
}

// ---- Index 24: DeleteLocalRef ----
extern "C" fn jni_delete_local_ref(_env: JNIEnv, lref: JObject) {
    // gcd d5/f: a leaf -- it touches only this thread's local-handle table
    // (the deposited snapshot keeps the object until the next deposit, an
    // over-retention only); see `ForeignJniEntry::enter_leaf`.
    let _fx = ForeignJniEntry::enter_leaf(lref);
    delete_local_ref(lref);
}

// ---- Index 25: IsSameObject ----
extern "C" fn jni_is_same_object(_env: JNIEnv, a: JObject, b: JObject) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    // A `jclass` does not resolve through `jobject_to_obj` (it is a tagged
    // `ClassId`, see `JCLASS_TAG`), so both sides fell into the
    // `(None, None) => JNI_TRUE` arm and EVERY pair of distinct classes
    // compared equal. Class handles are canonical — the same class always
    // yields the same tagged id — so compare them by id.
    //
    // Both sides go through `jni_identity` so that the two encodings a class
    // can arrive in compare equal: a `jclass` from `FindClass` and the same
    // class arriving as a `Class` PARAMETER are one object on HotSpot, and
    // `IsSameObject` between them is the idiom a native uses to dispatch on a
    // caller-supplied type. See `jclass_class_id` for what depended on it.
    match (jni_identity(a), jni_identity(b)) {
        (JniIdentity::Null, JniIdentity::Null) => JNI_TRUE,
        (JniIdentity::Class(ca), JniIdentity::Class(cb)) if ca == cb => JNI_TRUE,
        // Resolved through the global-ref layer, so a local ref and a global
        // ref to the same object compare equal.
        (JniIdentity::Object(ra), JniIdentity::Object(rb)) if ra == rb => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

// ---- Index 26: NewLocalRef ----
extern "C" fn jni_new_local_ref(_env: JNIEnv, obj: JObject) -> JObject {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    // A `jclass` is permanent and canonical; handing back a "local ref" to it
    // means handing back the same tagged id. Resolving it as an object would
    // yield 0 and lose the class.
    if is_jclass_handle(obj) {
        return obj;
    }
    // Resolve to a raw local-ref pointer (unwrap global-ref tag if present).
    match jobject_to_obj(obj) {
        Some(oref) => {
            let lref = obj_to_jobject(oref);
            if indirect_locals_active() {
                // gc-common w6-g: the same frame `track_local_ref` would use
                // (synthesized when none is open), handed out indirectly.
                JNI_LOCAL_FRAMES.with(|f| {
                    if let Ok(mut stack) = f.try_borrow_mut() {
                        if stack.is_empty() {
                            stack.push(Vec::with_capacity(16));
                        }
                    }
                });
                return record_local_handle_indirect(lref);
            }
            track_local_ref(lref);
            lref
        }
        None => 0,
    }
}

// ---- Index 27: EnsureLocalCapacity ----
///
/// Frames grow on demand, so any non-negative capacity is `JNI_OK`. A negative
/// one is refused with `OutOfMemoryError` pending and `JNI_ERR`, as HotSpot
/// does (gc-common w10-c; it answered `JNI_OK`).
extern "C" fn jni_ensure_local_capacity(_env: JNIEnv, capacity: JInt) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if capacity < 0 {
        refuse_local_capacity("EnsureLocalCapacity", capacity);
        return JNI_ERR;
    }
    JNI_OK
}

/// The `OutOfMemoryError` HotSpot leaves pending for a negative local
/// capacity (`PushLocalFrame` / `EnsureLocalCapacity`). No context: the
/// `ThrowNew` sentinel, so the refusal is still flagged.
fn refuse_local_capacity(what: &str, capacity: JInt) {
    let raised = with_shared_vm(|shared| {
        raise_jni_oom(shared, &format!("{what} capacity {capacity}"));
    });
    if raised.is_none() {
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
    }
}

// ---- Index 31: GetObjectClass ----
extern "C" fn jni_get_object_class(_env: JNIEnv, obj: JObject) -> JClass {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let class_id = match jni_array_class_id(shared, oref) {
            Some(array_class) => array_class,
            None => shared.mem.heap.class_id_of(oref),
        };
        Some(class_id_to_jclass(class_id))
    })
    .flatten()
    .unwrap_or(0)
}

/// The class `GetObjectClass` answers for an ARRAY object, or `None` when `oref`
/// is not an array.
///
/// An array's header carries its COMPONENT's class id (a `String[]` reads as
/// `java/lang/String`, and a primitive array as class 0), so the header id is
/// never the array's own class. Resolve the array class from its descriptor,
/// as `FindClass("[Ljava/lang/String;")` would. If that cannot be loaded, answer
/// `java/lang/Object`, a true supertype, never the component: with the
/// component, `IsInstanceOf(a, GetObjectClass(a))` is false.
fn jni_array_class_id(shared: &SharedVm, oref: ObjectRef) -> Option<ClassId> {
    let desc = crate::runtime::interpreter::array_descriptor_of(shared, oref)?;
    if let Ok(array_class) = shared.load_class_concurrent(&desc) {
        return Some(array_class);
    }
    shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/Object")
}

// ---- Index 32: IsInstanceOf ----
extern "C" fn jni_is_instance_of(_env: JNIEnv, obj: JObject, clazz: JClass) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return JNI_TRUE; // null is instanceof any type per JNI spec
    }
    if clazz == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let target_id = jclass_class_id(clazz);
        // An array's header carries its COMPONENT's class id, so the walk below
        // answered `String[] instanceof String` (and `Integer[] instanceof
        // Number`) true. Ask the `instanceof` opcode's own array rule.
        if let Some(desc) = crate::runtime::interpreter::array_descriptor_of(shared, oref) {
            let target_name = shared
                .classes
                .class_manager
                .read()
                .get_class(target_id)
                .map(|c| c.name.to_string())?;
            let is = crate::runtime::interpreter::array_is_instance_of(shared, &desc, &target_name);
            return Some(if is { JNI_TRUE } else { JNI_FALSE });
        }
        let obj_class_id = shared.mem.heap.class_id_of(oref);
        if obj_class_id == target_id {
            return Some(JNI_TRUE);
        }
        // Walk superclass chain
        let walked = {
            let cm = shared.classes.class_manager.read();
            let mut current = obj_class_id;
            loop {
                let class = cm.get_class(current)?;
                if class.interfaces.contains(&target_id) {
                    break true;
                }
                match class.superclass {
                    Some(sc) => {
                        if sc == target_id {
                            break true;
                        }
                        current = sc;
                    }
                    None => break false,
                }
            }
        };
        // The walk sees only the interfaces each class names DIRECTLY, so a
        // `Foo implements List` object was not an instance of `Collection`.
        // The subtype relation the opcodes use closes over super-interfaces.
        let is = walked
            || crate::runtime::interpreter::class_is_subtype(shared, obj_class_id, target_id);
        Some(if is { JNI_TRUE } else { JNI_FALSE })
    })
    .flatten()
    .unwrap_or(JNI_FALSE)
}

// ---- Index 33: GetMethodID ----
extern "C" fn jni_get_method_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JMethodID {
    let _fx = ForeignJniEntry::enter();
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    let sig_str = match unsafe { cstr_to_str(sig) } {
        Some(s) => s,
        None => return 0,
    };
    if clazz == 0 {
        return 0;
    }
    // Round 8 audit fix (CRIT #2): probe the per-VM `LinkResolver` to
    // dedupe the `(class, name, descriptor)` hierarchy walk. JNI
    // `GetMethodID` is hot on any C-extension entry path (Hibernate
    // ByteBuddy proxies, JNI-heavy libraries like SQLite-JDBC, native
    // image bridges) and the same triple is queried thousands of times
    // per cold start. Cache hits avoid the full `find_method_recursive`
    // walk + the per-class linear `methods` position scan.
    with_shared_vm(|shared| {
        use cratonvm_classloading::resolution::ResolvedMember;
        let class_id = jclass_class_id(clazz);
        let resolved = {
            let cm = shared.classes.class_manager.read();
            shared
                .classes
                .link_resolver
                .resolve_or_compute(class_id, name_str, sig_str, || {
                    let result =
                        find_method_recursive(class_id, name_str, sig_str, &cm.class_store)
                            .and_then(|(_, declaring)| {
                                let decl = cm.class_store.get(declaring)?;
                                let idx = decl.methods.iter().position(|m| {
                                    &*m.name == name_str && &*m.descriptor == sig_str
                                })?;
                                Some((declaring, idx as u32))
                            });
                    let resolved = match result {
                        Some((d, i)) => ResolvedMember::Method {
                            declaring_class_id: d,
                            index: i,
                        },
                        None => ResolvedMember::NotFound,
                    };
                    (
                        cratonvm_types::intern_arc(name_str),
                        cratonvm_types::intern_arc(sig_str),
                        resolved,
                    )
                })
        };
        match resolved {
            ResolvedMember::Method {
                declaring_class_id,
                index,
            } => Some(encode_method_id(declaring_class_id, index as u16)),
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Indices 34-63: Call<Type>MethodA (virtual instance dispatch)
// ---------------------------------------------------------------------------
//
// The varargs (`CallObjectMethod`) and va_list (`CallObjectMethodV`) forms
// cannot be implemented portably in safe Rust.  We provide the *A variants
// (array form) for all return types, plus stub no-ops at the varargs slots
// so that the function table is correctly sized.

extern "C" fn jni_call_object_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Object(Some(r))) => new_local_handle(r),
        _ => 0,
    }
}

extern "C" fn jni_call_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

extern "C" fn jni_call_byte_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

extern "C" fn jni_call_char_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

extern "C" fn jni_call_short_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

extern "C" fn jni_call_int_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

extern "C" fn jni_call_long_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

extern "C" fn jni_call_float_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

extern "C" fn jni_call_double_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

extern "C" fn jni_call_void_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) {
    let _fx = ForeignJniEntry::enter();
    jni_call_instance(obj, mid, args);
}

// ---------------------------------------------------------------------------
// Indices 64-93: CallNonvirtual<Type>MethodA
// ---------------------------------------------------------------------------

extern "C" fn jni_call_nonvirtual_object_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Object(Some(r))) => new_local_handle(r),
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_byte_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_char_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_short_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_int_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_long_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_float_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

extern "C" fn jni_call_nonvirtual_double_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

extern "C" fn jni_call_nonvirtual_void_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) {
    let _fx = ForeignJniEntry::enter();
    jni_call_nonvirtual(obj, clazz, mid, args);
}

// ---------------------------------------------------------------------------
// Indices 114-143: CallStatic<Type>MethodA
// ---------------------------------------------------------------------------

extern "C" fn jni_call_static_object_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Object(Some(r))) => new_local_handle(r),
        _ => 0,
    }
}

extern "C" fn jni_call_static_boolean_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

extern "C" fn jni_call_static_byte_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

extern "C" fn jni_call_static_char_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

extern "C" fn jni_call_static_short_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

extern "C" fn jni_call_static_int_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

extern "C" fn jni_call_static_long_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

extern "C" fn jni_call_static_float_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

extern "C" fn jni_call_static_double_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

extern "C" fn jni_call_static_void_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) {
    let _fx = ForeignJniEntry::enter();
    jni_call_static(clazz, mid, args);
}

// ---- Index 94: GetFieldID ----
extern "C" fn jni_get_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
    let _fx = ForeignJniEntry::enter();
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    // Round 9 audit fix (HIGH #5): include the JNI-supplied signature in
    // the LinkResolver cache key. The previous version dropped the
    // signature and keyed only on `(class_id, name)`; for a class that
    // shadows an inherited field with a different *type* (legal in JVMS
    // §5.4.3.2 — fields are uniquely identified by `(name, descriptor)`),
    // the first JNI `GetFieldID` query would populate the cache with one
    // resolution and every subsequent call — even one passing a
    // different signature — would short-circuit to that wrong entry.
    //
    // Tolerate a null signature by mapping it to "" — pre-fix callers
    // could legitimately pass NULL since the parameter was ignored; we
    // keep that behaviour by treating NULL as a distinct ("no signature
    // assertion") cache key.
    let sig_str = unsafe { cstr_to_str(sig) }.unwrap_or("");
    if clazz == 0 {
        return 0;
    }
    // Round 8 audit fix (CRIT #2): probe the per-VM `LinkResolver`.
    with_shared_vm(|shared| {
        use cratonvm_classloading::resolution::ResolvedMember;
        let class_id = jclass_class_id(clazz);
        let resolved = {
            let cm = shared.classes.class_manager.read();
            shared
                .classes
                .link_resolver
                .resolve_or_compute(class_id, name_str, sig_str, || {
                    // Round 9 fixed the cache KEY to include the signature; the
                    // resolution underneath it still discarded the signature and
                    // matched on the name alone, so two `GetFieldID` calls with
                    // different signatures got two cache entries holding the
                    // same (possibly wrong) field. JVMS §5.4.3.2 — and the
                    // comment above — say the key is `(name, descriptor)`; use
                    // it. An empty `sig_str` is the documented NULL-signature
                    // case and keeps the name-only search.
                    //
                    // Falls back to name-only, counted, when no field of that
                    // exact pair exists, for the reason `locate_field` does.
                    let result = if sig_str.is_empty() {
                        find_field_recursive(class_id, name_str, &cm.class_store)
                    } else {
                        cratonvm_classloading::find_field_recursive_by_descriptor(
                            class_id,
                            name_str,
                            sig_str,
                            &cm.class_store,
                        )
                        .or_else(|| {
                            let name_only =
                                find_field_recursive(class_id, name_str, &cm.class_store);
                            if name_only.is_some() {
                                crate::runtime::resolve::FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            name_only
                        })
                    };
                    let resolved = match result {
                        Some((field_index, field, declaring)) => ResolvedMember::Field {
                            declaring_class_id: declaring,
                            absolute_index: field_index as u32,
                            is_static: field.access_flags.contains(
                                cratonvm_reader::class_access_flags::FieldAccessFlags::STATIC,
                            ),
                        },
                        None => ResolvedMember::NotFound,
                    };
                    (
                        cratonvm_types::intern_arc(name_str),
                        cratonvm_types::intern_arc(sig_str),
                        resolved,
                    )
                })
        };
        match resolved {
            ResolvedMember::Field {
                declaring_class_id,
                absolute_index,
                ..
            } => Some(encode_field_id(declaring_class_id, absolute_index as usize)),
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 95: GetObjectField ----
extern "C" fn jni_get_object_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JObject {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Object(Some(r)) => Some(new_local_handle(r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 96: GetBooleanField ----
extern "C" fn jni_get_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JBoolean
}

// ---- Index 97: GetByteField ----
extern "C" fn jni_get_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JByte {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JByte
}

// ---- Index 98: GetCharField ----
extern "C" fn jni_get_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JChar {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JChar
}

// ---- Index 99: GetShortField ----
extern "C" fn jni_get_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JShort {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JShort
}

// ---- Index 100: GetIntField ----
extern "C" fn jni_get_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JInt {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id)
}

// ---- Index 101: GetLongField ----
extern "C" fn jni_get_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JLong {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Long(l) => Some(l),
            Value::Int(i) => Some(i as JLong),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 102: GetFloatField ----
extern "C" fn jni_get_float_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Float(f) => Some(f),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Index 103: GetDoubleField ----
extern "C" fn jni_get_double_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Double(d) => Some(d),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Index 104: SetObjectField ----
extern "C" fn jni_set_object_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JObject) {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        // The value first: a `jclass` value may mint its mirror
        // (`jclass_handle_mirror`), and `oref` must not be held across that.
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared.mem.heap.set_field(oref, field_index, value);
        // write_barrier fires automatically inside set_field
        Some(())
    });
}

// ---- Index 105: SetBooleanField ----
extern "C" fn jni_set_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JBoolean) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 106: SetByteField ----
extern "C" fn jni_set_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JByte) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 107: SetCharField ----
extern "C" fn jni_set_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JChar) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 108: SetShortField ----
extern "C" fn jni_set_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JShort) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 109: SetIntField ----
extern "C" fn jni_set_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JInt) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val);
}

// ---- Index 110: SetLongField ----
extern "C" fn jni_set_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JLong) {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        // Long-smuggle mint chokepoint: native code storing a raw jobject
        // handle into a Java long field (see smuggled_longs). Strict probe so
        // ordinary numeric stores don't register.
        let bits = val as u64;
        if bits != 0
            && bits & 0x7 == 0
            && shared.mem.heap.is_object_address(bits as usize).is_some()
        {
            crate::memory::smuggled_longs::record_minted_long(&shared.mem.heap, bits);
        }
        shared
            .mem
            .heap
            .set_field(oref, field_index, Value::Long(val));
        Some(())
    });
}

// ---- Index 111: SetFloatField ----
extern "C" fn jni_set_float_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JFloat) {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .mem
            .heap
            .set_field(oref, field_index, Value::Float(val));
        Some(())
    });
}

// ---- Index 112: SetDoubleField ----
extern "C" fn jni_set_double_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JDouble) {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .mem
            .heap
            .set_field(oref, field_index, Value::Double(val));
        Some(())
    });
}

// ---- Index 113: GetStaticMethodID ----
extern "C" fn jni_get_static_method_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JMethodID {
    let _fx = ForeignJniEntry::enter();
    // Same resolution as instance methods — static dispatch is by method index.
    jni_get_method_id(_env, clazz, name, sig)
}

// ---- Index 144: GetStaticFieldID ----
extern "C" fn jni_get_static_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
    let _fx = ForeignJniEntry::enter();
    jni_get_field_id(_env, clazz, name, sig)
}

// ---- Indices 145-150: GetStatic*Field ----
extern "C" fn jni_get_static_object_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Object(Some(r))) => Some(new_local_handle(*r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

extern "C" fn jni_get_static_boolean_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JBoolean
}

extern "C" fn jni_get_static_byte_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JByte {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JByte
}

extern "C" fn jni_get_static_char_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JChar {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JChar
}

extern "C" fn jni_get_static_short_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JShort
}

extern "C" fn jni_get_static_int_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JInt {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id)
}

extern "C" fn jni_get_static_long_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JLong {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Long(l)) => Some(*l),
            Some(Value::Int(i)) => Some(*i as JLong),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

extern "C" fn jni_get_static_float_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Float(f)) => Some(*f),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

extern "C" fn jni_get_static_double_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Double(d)) => Some(*d),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Indices 154-159: SetStatic*Field ----
extern "C" fn jni_set_static_object_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JObject,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                // SATB pre-barrier for the overwritten reference, as
                // `set_static_shared` does: statics live outside the heap, so
                // no collector-side barrier sees this store (2026-09-23).
                shared.mem.heap.satb_barrier(fields[field_index]);
                fields[field_index] = value;
            }
        }
    });
}

extern "C" fn jni_set_static_int_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JInt,
) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(field_id, val);
}

extern "C" fn jni_set_static_long_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JLong,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        // Mint chokepoint, the static twin of `SetLongField`'s.
        mint_if_smuggled_jlong(&shared.mem.heap, val as u64);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Long(val);
            }
        }
    });
}

extern "C" fn jni_set_static_float_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JFloat,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Float(val);
            }
        }
    });
}

extern "C" fn jni_set_static_double_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JDouble,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Double(val);
            }
        }
    });
}

// ---- Index 167: NewStringUTF ----
extern "C" fn jni_new_string_utf(_env: JNIEnv, chars: *const c_char) -> JString {
    // gcd d10/j: allocates without a collection and runs no Java; its
    // `OutOfMemoryError` goes through `with_jni_context`, which escalates the
    // entry (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    let s = match unsafe { cstr_to_str(chars) } {
        Some(s) => s,
        None => return 0,
    };
    // gc-common w7-c: a NEW, un-interned String, allocated fallibly. This used
    // to be `create_java_string`, which INTERNS: every string a native built
    // went into `string_pool`, which `roots.rs` section 5 roots strongly, so
    // it could never be collected (a native formatting a fresh message per
    // call leaked one String per call for the life of the VM), two
    // `NewStringUTF("x")` calls returned the SAME object where HotSpot returns
    // two, and a full heap aborted the process instead of answering NULL
    // with `OutOfMemoryError` pending (`jni_alloc_or_oom`, as every other
    // allocating JNI function does since w5-g).
    with_shared_vm(|shared| {
        match jni_alloc_or_oom(shared, "a java/lang/String", |_| {
            crate::vm::try_create_java_string_uninterned(shared, s)
        }) {
            Some(obj) => new_local_handle(obj),
            None => 0,
        }
    })
    .unwrap_or(0)
}

// ---- Index 168: GetStringUTFLength ----
extern "C" fn jni_get_string_utf_length(_env: JNIEnv, str_obj: JString) -> JSize {
    // gcd d5/f: a leaf (reads the String and its value array into Rust
    // memory); see `ForeignJniEntry::enter_leaf`.
    let _fx = ForeignJniEntry::enter_leaf(str_obj);
    if str_obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        // The JNI contract specifies the *modified* UTF-8 length, which must
        // agree with what GetStringUTFChars produces (a caller commonly does
        // `malloc(GetStringUTFLength()+1)` then copies the chars in). Standard
        // `s.len()` undercounts interior NULs (1→2 bytes) and supplementary
        // chars (4→6 bytes), so compute the modified-UTF-8 length explicitly.
        Some(modified_utf8_len(&s) as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 169: GetStringUTFChars ----
extern "C" fn jni_get_string_utf_chars(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const c_char {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if str_obj == 0 {
        return std::ptr::null();
    }
    let result = with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        // Encode as JNI "modified UTF-8": interior NUL (U+0000) must be encoded
        // as the two-byte sequence 0xC0 0x80 rather than a single 0x00, so that
        // the C string is only terminated by the trailing NUL we append below.
        // (Plain CString::new() would reject any embedded NUL and return None,
        // violating the JNI contract for strings that contain U+0000.)
        let modified = to_modified_utf8(&s);
        // `modified` contains no interior NUL bytes, so CString::new never fails;
        // it appends the single terminating NUL. Caller frees via
        // ReleaseStringUTFChars (CString::from_raw), which round-trips cleanly.
        let c_string = std::ffi::CString::new(modified).ok()?;
        Some(c_string.into_raw() as *const c_char)
    })
    .flatten();
    match result {
        Some(ptr) => {
            if !is_copy.is_null() {
                unsafe {
                    *is_copy = JNI_TRUE;
                }
            }
            ptr
        }
        None => std::ptr::null(),
    }
}

// ---- Index 170: ReleaseStringUTFChars ----
extern "C" fn jni_release_string_utf_chars(_env: JNIEnv, _str: JString, chars: *const c_char) {
    if !chars.is_null() {
        // Free the CString allocated by GetStringUTFChars
        unsafe {
            drop(std::ffi::CString::from_raw(chars as *mut c_char));
        }
    }
}

// ---- Index 171: GetArrayLength ----
extern "C" fn jni_get_array_length(_env: JNIEnv, array: JArray) -> JSize {
    // gcd d5/f: a leaf (a header read); see `ForeignJniEntry::enter_leaf`.
    let _fx = ForeignJniEntry::enter_leaf(array);
    if array == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        Some(shared.mem.heap.array_length(oref) as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 172: NewObjectArray ----
extern "C" fn jni_new_object_array(
    _env: JNIEnv,
    length: JSize,
    clazz: JClass,
    init: JObject,
) -> JArray {
    let _fx = ForeignJniEntry::enter();
    if length < 0 {
        raise_jni_negative_array_size(length);
        return 0;
    }
    with_shared_vm(|shared| {
        let component_id = jclass_class_id(clazz);
        let Some(arr) = jni_alloc_or_oom(shared, "an object array", |heap| {
            heap.try_alloc_array_full(component_id, ArrayElementType::Reference, length as usize)
        }) else {
            return 0;
        };
        // Initialize elements if init is non-null
        if init != 0 {
            if let Some(init_ref) = jobject_to_obj(init) {
                for i in 0..length as usize {
                    let _ =
                        shared
                            .mem
                            .heap
                            .set_array_element(arr, i, Value::Object(Some(init_ref)));
                }
            }
        }
        new_local_handle(arr)
    })
    .unwrap_or(0)
}

// ---- Index 173: GetObjectArrayElement ----
extern "C" fn jni_get_object_array_element(_env: JNIEnv, array: JArray, index: JSize) -> JObject {
    // gcd d10/j: runs no Java on its ordinary path; the out-of-bounds raise
    // escalates through `with_jni_context` (see
    // `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if array == 0 {
        return 0;
    }
    // `Err(length)`: `index` is outside the array. HotSpot throws
    // `ArrayIndexOutOfBoundsException` here; this returned NULL with nothing
    // pending, which a native reads as a stored null. Raised after the
    // context borrow is released (round 12 wave 1, lane rt).
    let got = with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        if let Some(length) = jni_array_index_out_of_bounds(shared, oref, index) {
            return Some(Err(length));
        }
        Some(Ok(match shared.mem.heap.get_array_element(oref, index as usize) {
            Ok(Value::Object(Some(r))) => new_local_handle(r),
            _ => 0,
        }))
    })
    .flatten();
    match got {
        Some(Ok(handle)) => handle,
        Some(Err(length)) => {
            raise_jni_index_out_of_bounds(index, length);
            0
        }
        None => 0,
    }
}

/// `Some(length)` when `oref` is an array and `index` is outside it; `None`
/// when the index is in bounds, and for a non-array (which the element
/// accessors below refuse on their own, as before).
fn jni_array_index_out_of_bounds(shared: &SharedVm, oref: ObjectRef, index: JSize) -> Option<usize> {
    if shared.mem.heap.kind_of(oref) != crate::memory::heap::ObjectKind::Array {
        return None;
    }
    let length = shared.mem.heap.array_length(oref);
    // Cast: a non-negative `JSize` fits `usize`.
    (index < 0 || index as usize >= length).then_some(length)
}

/// A pending `ArrayIndexOutOfBoundsException` with HotSpot's JNI message for
/// a signed element index (`Index -1 out of bounds for length 3`);
/// [`raise_jni_aioobe`] is the region form, whose start is never negative.
fn raise_jni_index_out_of_bounds(index: JSize, length: usize) {
    raise_jni_throwable(
        "java/lang/ArrayIndexOutOfBoundsException",
        &format!("Index {index} out of bounds for length {length}"),
    );
}

/// `New<Type>Array` / `NewObjectArray` with a negative length: HotSpot leaves
/// `NegativeArraySizeException` pending (message: the length), as `newarray`
/// throws. This returned NULL with nothing pending, which a native reads as an
/// allocation failure (round 12 wave 1, lane rt).
fn raise_jni_negative_array_size(length: JSize) {
    raise_jni_throwable(
        "java/lang/NegativeArraySizeException",
        &length.to_string(),
    );
}

/// Leave a `class_name` throwable with `msg` pending for the current JNI call,
/// or the `ThrowNew` sentinel when none can be built (as
/// [`raise_jni_aioobe`] does).
fn raise_jni_throwable(class_name: &str, msg: &str) {
    let raised = with_jni_context(|shared, thread| {
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            class_name,
            Some(msg),
        ) {
            Ok(exc) => {
                set_jni_pending_exception_object(exc);
                Ok(())
            }
            Err(e) => Err(is_heap_exhaustion(&e)),
        }
    })
    .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        pend_unbuilt_throwable(heap_exhausted);
    }
}

/// gcd d4/k: did `create_exception_object` fail for want of heap (as opposed
/// to a class that cannot be loaded, or no thread context)?
fn is_heap_exhaustion(e: &crate::error::MethodCallFailed) -> bool {
    matches!(
        e,
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::OutOfMemoryError { .. }
        ))
    )
}

/// gcd d4/k (`docs/internal/gc/gcd-d3k-jni-exception-sentinel-escapes-to-native-FIXED-20260928.md`):
/// leave pending what a JNI function could not build. When the build failed
/// for want of heap, that is the VM's preallocated `OutOfMemoryError`, as
/// HotSpot throws when it cannot allocate the exception it meant to throw --
/// a real object, so `ExceptionOccurred` hands the native a real throwable.
/// Otherwise (no thread context, a class that will not load, no singleton
/// yet) the `u64::MAX` sentinel, as before: flagged, never a silent success,
/// and turned into a VM-internal failure at the native's return.
fn pend_unbuilt_throwable(heap_exhausted: bool) {
    if heap_exhausted {
        if let Some(oom) = with_shared_vm(|shared| *shared.mem.singleton_oom.read()).flatten() {
            set_jni_pending_exception_object(oom);
            return;
        }
    }
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
}

// ---- Index 174: SetObjectArrayElement ----
extern "C" fn jni_set_object_array_element(
    _env: JNIEnv,
    array: JArray,
    index: JSize,
    val: JObject,
) {
    let _fx = ForeignJniEntry::enter();
    if array == 0 {
        return;
    }
    // HotSpot throws `ArrayIndexOutOfBoundsException` for an index outside
    // the array and `ArrayStoreException` for a value that is not an instance
    // of the component; both were silently dropped here (the store just did
    // not happen). Raised after the context borrow is released (round 12
    // wave 1, lane rt).
    enum Refused {
        Index(usize),
        Store(String),
    }
    let refused = with_shared_vm(|shared| {
        // The value first, as in `SetObjectField`: it may mint a mirror.
        let value = jobject_to_obj(val);
        let oref = jobject_to_obj(array)?;
        if let Some(length) = jni_array_index_out_of_bounds(shared, oref, index) {
            return Some(Refused::Index(length));
        }
        // The shared `aastore` rule (fails open on imprecise types, like the
        // opcode), asked only of a reference array.
        if let Some(v) = value {
            if shared.mem.heap.element_type_of(oref) == ArrayElementType::Reference
                && !crate::runtime::interpreter::aastore_element_assignable(shared, oref, v)
            {
                return Some(Refused::Store(jni_array_store_message(shared, oref, v, index)));
            }
        }
        let _ = shared
            .mem
            .heap
            .set_array_element(oref, index as usize, Value::Object(value));
        None
    })
    .flatten();
    match refused {
        Some(Refused::Index(length)) => raise_jni_index_out_of_bounds(index, length),
        Some(Refused::Store(msg)) => raise_jni_throwable("java/lang/ArrayStoreException", &msg),
        None => {}
    }
}

/// HotSpot's `SetObjectArrayElement` refusal text: `type mismatch: can not
/// store java.lang.String to java.lang.Integer[0]`, the array written as its
/// bottom component with one `[]` per further dimension (`int[0][]`).
fn jni_array_store_message(
    shared: &SharedVm,
    array: ObjectRef,
    value: ObjectRef,
    index: JSize,
) -> String {
    let value_name = match crate::runtime::interpreter::array_descriptor_of(shared, value) {
        Some(desc) => desc.replace('/', "."),
        None => shared
            .classes
            .class_manager
            .read()
            .get_class(shared.mem.heap.class_id_of(value))
            .map(|c| c.name.replace('/', "."))
            .unwrap_or_default(),
    };
    let desc = crate::runtime::interpreter::array_descriptor_of(shared, array).unwrap_or_default();
    let dims = desc.bytes().take_while(|&b| b == b'[').count();
    let bottom = desc.get(dims..).unwrap_or("");
    let bottom_name = match bottom {
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "S" => "short".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        other => other
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
            .unwrap_or(other)
            .replace('/', "."),
    };
    let mut msg = format!("type mismatch: can not store {value_name} to {bottom_name}[{index}]");
    for _ in 1..dims {
        msg.push_str("[]");
    }
    msg
}

// ---- Indices 175-181: New<Type>Array ----
macro_rules! new_prim_array {
    ($name:ident, $elem_type:expr) => {
        extern "C" fn $name(_env: JNIEnv, length: JSize) -> JArray {
            // gcd d10/j: allocates without a collection and runs no Java; a
            // raise escalates through `with_jni_context` (see
            // `ForeignJniEntry::enter_vm_only`).
            let _fx = ForeignJniEntry::enter_vm_only();
            if length < 0 {
                raise_jni_negative_array_size(length);
                return 0;
            }
            with_shared_vm(|shared| {
                match jni_alloc_or_oom(shared, "a primitive array", |heap| {
                    heap.try_alloc_array_full(ClassId::new(0), $elem_type, length as usize)
                }) {
                    Some(arr) => new_local_handle(arr),
                    None => 0,
                }
            })
            .unwrap_or(0)
        }
    };
}

new_prim_array!(jni_new_boolean_array, ArrayElementType::Boolean); // 175
new_prim_array!(jni_new_byte_array, ArrayElementType::Byte); // 176
new_prim_array!(jni_new_char_array, ArrayElementType::Char); // 177
new_prim_array!(jni_new_short_array, ArrayElementType::Short); // 178
new_prim_array!(jni_new_int_array, ArrayElementType::Int); // 179
new_prim_array!(jni_new_long_array, ArrayElementType::Long); // 180
new_prim_array!(jni_new_float_array, ArrayElementType::Float); // 181
new_prim_array!(jni_new_double_array, ArrayElementType::Double); // 182

// ---- Indices 183-190: Get<Type>ArrayElements ----
// Returns a pointer to a native-endian COPY of the array data.
//
// GC-correctness (vm-jni-roots #2): we ALWAYS hand native code a detached copy
// of the array body and report `is_copy = JNI_TRUE` — never a raw pointer into
// the live, in-heap array. This VM ships *moving* collectors (the generational
// young-gen copy and the G1 evacuator both relocate live objects); a direct
// body pointer handed to C would dangle the instant a GC fired while native
// code held it — a use-after-free / heap-corruption hole. The JNI spec
// explicitly permits returning a copy (`is_copy = JNI_TRUE`), so a copy is the
// only collector-agnostic correct choice for this heap. (HotSpot can return a
// direct pointer because it pins the array's page/region for the window; this
// VM closes the same gap with the copy instead. A prior revision handed out the
// direct `array_data_ptr` for ordinary arrays on a "heap never moves" premise
// the VM does not actually hold — that was the bug this restores.)
//
// The copy is built via the region-safe per-element accessor, so it works
// uniformly for ordinary contiguous arrays AND G1 *humongous* arrays (whose
// payload spans non-contiguous regions and have no flat data pointer). It is
// registered in the VM's `JniGlobalRefs::elem_copies` (per VM since gc-common
// w10-c, so a release from any thread finds it) and the matching
// `Release<Type>ArrayElements` copies any mutations back and frees it.
//
// Keep-alive AND copy-back correctness under a MOVING GC: at Get we mint a JNI
// **global ref** for the SOURCE array and stash its handle in the buffer's
// `elem_copies` record. A global ref is both (a) a GC root — so the
// array cannot be reclaimed before the copy-back at Release — and, crucially,
// (b) *remappable*: `collect_roots` scans it and `update_after_gc` /
// `update_all_roots` rewrite the boxed `ObjectRef` to the object's new address
// after every relocating collection (generational young-copy and G1 evacuation
// alike). So `Release<Type>ArrayElements` resolves the array's CURRENT location
// through that handle, and the (possibly mutated) copy is written back into the
// live array — never a freed CSet region (silent data loss) or a recycled
// object (corruption). This replaces the previous keep-alive object-pin for
// this path, which was address-keyed and therefore went stale under a move; the
// global ref is deleted on the final Release. (The JNI spec permits holding the
// copy arbitrarily long, so region pinning — used by the *critical* path — is
// the wrong tool here: it would starve the collector for the whole window. A
// remappable handle imposes no such no-relocation constraint.)
macro_rules! get_array_elements {
    ($name:ident, $rust_type:ty, $value_variant:ident, $default:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            is_copy: *mut JBoolean,
        ) -> *mut $rust_type {
            let _fx = ForeignJniEntry::enter();
            if array == 0 {
                return std::ptr::null_mut();
            }
            let result = with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                // Materialise a detached, native-endian copy via the region-safe
                // per-element accessor (works for ordinary AND G1-humongous
                // arrays). See the module comment above for why a copy — not a
                // direct heap pointer — is what we hand to native code.
                let len = shared.mem.heap.array_length(oref);
                // `len.max(1)` guarantees a real, uniquely-addressed allocation
                // even for a zero-length array, so its buffer pointer is never a
                // shared dangling sentinel that would collide in the maps below.
                // Release uses the STORED capacity (not `len`), so over-allocating
                // by one element for the empty case stays sound.
                //
                // gc-common w10-c: reserved FALLIBLY. `Vec::with_capacity`
                // aborts the process when the C heap cannot hold the copy (a
                // `long[]` of a few hundred million elements); JNI's answer is
                // NULL with `OutOfMemoryError` pending, as HotSpot gives when
                // its malloc of the copy fails.
                let mut buf: Vec<$rust_type> = Vec::new();
                if buf.try_reserve_exact(len.max(1)).is_err() {
                    raise_jni_oom(shared, "a Get<Type>ArrayElements copy");
                    return None;
                }
                for i in 0..len {
                    let val = match shared.mem.heap.get_array_element(oref, i) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let elem = match val {
                        Value::$value_variant(v) => v as $rust_type,
                        _ => $default,
                    };
                    buf.push(elem);
                }
                let ptr = buf.as_mut_ptr();
                // Record (ptr -> (len, cap)) so Release uses the EXACT layout we
                // allocated. `buf.len()` bounds the copy-back (never read an
                // uninitialised tail) and `buf.capacity()` is the size
                // `Vec::from_raw_parts` requires for a sound free.
                let buf_len = buf.len();
                let buf_cap = buf.capacity();
                // Mint a REMAPPABLE keep-alive handle for the source array: a JNI
                // global ref. It keeps the array alive for the whole Get/Release
                // window AND is rewritten by the GC on every relocating
                // collection, so the copy-back at Release follows the array to
                // its current address (see the module comment above). Deleted on
                // the final Release. Recorded in the VM's table, under the same
                // lock, so a Release on ANY thread finds it (gc-common w10-c).
                shared.natives.jni_global_refs.lock().open_elem_copy(
                    ptr as usize,
                    buf_len,
                    buf_cap,
                    oref,
                );
                std::mem::forget(buf); // OWNERSHIP: buffer transferred to native caller, freed by Release<Type>ArrayElements via Vec::from_raw_parts
                Some((ptr, JNI_TRUE))
            })
            .flatten();
            match result {
                Some((ptr, copy_flag)) => {
                    if !is_copy.is_null() {
                        unsafe {
                            *is_copy = copy_flag;
                        }
                    }
                    ptr
                }
                None => std::ptr::null_mut(),
            }
        }
    };
}

get_array_elements!(jni_get_boolean_array_elements, JBoolean, Int, 0); // 183
get_array_elements!(jni_get_byte_array_elements, JByte, Int, 0); // 184
get_array_elements!(jni_get_char_array_elements, JChar, Int, 0); // 185
get_array_elements!(jni_get_short_array_elements, JShort, Int, 0); // 186
get_array_elements!(jni_get_int_array_elements, JInt, Int, 0); // 187
get_array_elements!(jni_get_long_array_elements, JLong, Long, 0); // 188
get_array_elements!(jni_get_float_array_elements, JFloat, Float, 0.0); // 189
get_array_elements!(jni_get_double_array_elements, JDouble, Double, 0.0); // 190

// ---- Indices 191-198: Release<Type>ArrayElements ----
// Frees the buffer allocated by Get<Type>ArrayElements and optionally copies back.
macro_rules! release_array_elements {
    ($name:ident, $rust_type:ty, $value_constructor:expr) => {
        extern "C" fn $name(_env: JNIEnv, array: JArray, elems: *mut $rust_type, mode: JInt) {
            let _fx = ForeignJniEntry::enter();
            // The raw `array` handle the caller passes back is deliberately NOT
            // trusted to locate the array: it is a from-space pointer that a
            // moving GC may have invalidated during the Get/Release window. We
            // resolve the array through the remappable global ref recorded at Get
            // (see below), which the GC keeps current across relocations.
            let _ = array;
            if elems.is_null() {
                return;
            }
            // BUG FIX (vm-jni-roots #2): look up the `ArrayElemBuffer` recorded
            // for THIS buffer at Get time. Never re-derive the length from the
            // array handle — the array may have moved/realloc'd under a moving
            // GC, the handle may be aliased/stale, or `array_length` may read 0,
            // any of which made the old code build the copy-back loop and
            // `Vec::from_raw_parts` with a wrong length → heap corruption.
            //
            // For mode != 1 (i.e. modes that free) we `remove` the entry so the
            // pointer can never be double-freed; for JNI_COMMIT (1, no free) we
            // only `get` so a later release can still find it (and its global
            // ref stays alive for that later release).
            //
            // gc-common w10-c: the record is the VM's, not this thread's, so a
            // release on another thread than the Get's finds it. No context
            // (no VM to look in) is a miss, as an unknown pointer is.
            let entry = with_shared_vm(|shared| {
                shared
                    .natives
                    .jni_global_refs
                    .lock()
                    .elem_copy(elems as usize, mode != 1)
            })
            .flatten();
            let ArrayElemBuffer {
                len: stored_len,
                cap: stored_cap,
                array_gref,
            } = match entry {
                Some(v) => v,
                // Unknown pointer — not one we handed out (or already released).
                // Do nothing rather than risk a wrong-length free.
                None => return,
            };
            // mode 0 = copy back and free, JNI_COMMIT = copy back don't free,
            // JNI_ABORT = free without copy back
            if mode != 2 {
                // Copy back to the array (mode 0 or JNI_COMMIT=1). Resolve the
                // array's CURRENT address through the remappable global ref — the
                // GC rewrote it to follow any relocation since Get, so the write
                // lands in the live array, not a freed/recycled region. Bound the
                // loop by BOTH our initialised length and the live array length so
                // we neither read past the end of our buffer nor write out of the
                // array's bounds if it has since shrunk.
                with_shared_vm(|shared| {
                    let oref = shared.natives.jni_global_refs.lock().resolve(array_gref)?;
                    let arr_len = shared.mem.heap.array_length(oref);
                    let copy_len = stored_len.min(arr_len);
                    for i in 0..copy_len {
                        let val = unsafe { *elems.add(i) };
                        let value = $value_constructor(val);
                        // Mint chokepoint for the `jlong[]` instantiation
                        // (`SetLongArrayRegion`'s twin); a no-op for the rest.
                        mint_if_smuggled_long_value(&shared.mem.heap, &value);
                        let _ = shared.mem.heap.set_array_element(oref, i, value);
                    }
                    Some(())
                });
            }
            if mode != 1 {
                // Final (freeing) release (mode 0 or JNI_ABORT=2): delete the
                // remappable global ref (drops the keep-alive root for the array)
                // and free the buffer using the EXACT length and capacity we
                // recorded at allocation time. On JNI_COMMIT (1) both the entry
                // and its global ref intentionally persist for a later release.
                with_shared_vm(|shared| {
                    shared.natives.jni_global_refs.lock().remove(array_gref);
                    Some(())
                });
                unsafe {
                    drop(Vec::from_raw_parts(elems, stored_len, stored_cap));
                }
            }
        }
    };
}

release_array_elements!(
    jni_release_boolean_array_elements,
    JBoolean,
    |v: JBoolean| Value::Int(v as i32)
); // 191
release_array_elements!(jni_release_byte_array_elements, JByte, |v: JByte| {
    Value::Int(v as i32)
}); // 192
release_array_elements!(jni_release_char_array_elements, JChar, |v: JChar| {
    Value::Int(v as i32)
}); // 193
release_array_elements!(jni_release_short_array_elements, JShort, |v: JShort| {
    Value::Int(v as i32)
}); // 194
release_array_elements!(jni_release_int_array_elements, JInt, |v: JInt| Value::Int(
    v
)); // 195
release_array_elements!(jni_release_long_array_elements, JLong, |v: JLong| {
    Value::Long(v)
}); // 196
release_array_elements!(jni_release_float_array_elements, JFloat, |v: JFloat| {
    Value::Float(v)
}); // 197
release_array_elements!(jni_release_double_array_elements, JDouble, |v: JDouble| {
    Value::Double(v)
}); // 198

// ---------------------------------------------------------------------------
// Region bounds-checking (JNI Get/Set<Type>ArrayRegion + string regions)
// ---------------------------------------------------------------------------
//
// The JNI spec mandates that `Get/Set<Type>ArrayRegion` and the string-region
// accessors throw `ArrayIndexOutOfBoundsException` (resp. `StringIndexOutOf-
// BoundsException`, which we surface as AIOOBE) when `start`/`len` fall outside
// the array/string. Previously these helpers relied only on the per-element
// accessor's *internal* bounds check, which silently swallowed an out-of-range
// request (reading/writing nothing) instead of signalling the contract error.
// `region_bounds_ok` performs the explicit, overflow-safe check; on failure it
// raises a pending AIOOBE so the interpreter throws it on native return.

/// Set a pending `ArrayIndexOutOfBoundsException` for the current JNI call.
///
/// When a `JvmThread` context is available we materialise a real exception
/// object and store its handle in `JNI_PENDING_EXCEPTION` (the same slot
/// `Throw`/`ThrowNew` use), so `vm_exec` rethrows it as a catchable Java
/// exception on return from the native call. Without a thread context (e.g. a
/// direct unit-test call) we fall back to the `ThrowNew` sentinel so the error
/// is still flagged rather than silently dropped.
fn raise_jni_aioobe(index: usize, length: usize) {
    let msg = format!("Index {index} out of bounds for length {length}");
    let raised =
        with_jni_context(
            |shared, thread| match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/ArrayIndexOutOfBoundsException",
                Some(&msg),
            ) {
                Ok(exc) => {
                    set_jni_pending_exception_object(exc);
                    Ok(())
                }
                Err(e) => Err(is_heap_exhaustion(&e)),
            },
        )
        .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        // No thread context or allocation failed: the preallocated OOME for a
        // heap failure, else the sentinel, so the condition is not silently
        // swallowed (gcd d4/k, `pend_unbuilt_throwable`).
        pend_unbuilt_throwable(heap_exhausted);
    }
}

/// Validate that the `[start, start + len)` window lies within `[0, length)`.
///
/// `start`/`len` are caller-supplied `JSize` (i32). Returns `true` for an
/// in-bounds (or empty) region and `false` otherwise; on `false` a pending
/// AIOOBE has been raised. Uses checked arithmetic so `start + len` cannot
/// overflow and wrap into a spuriously-valid range.
fn region_bounds_ok(start: JSize, len: JSize, length: usize) -> bool {
    if start < 0 || len < 0 {
        raise_jni_aioobe(start.max(0) as usize, length);
        return false;
    }
    let start = start as usize;
    let len = len as usize;
    match start.checked_add(len) {
        Some(end) if end <= length => true,
        _ => {
            raise_jni_aioobe(start, length);
            false
        }
    }
}

// ---- Indices 199-206: Get<Type>ArrayRegion ----
macro_rules! get_array_region {
    ($name:ident, $rust_type:ty, $value_variant:ident, $default:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            start: JSize,
            len: JSize,
            buf: *mut $rust_type,
        ) {
            // gcd d5/f: a leaf (primitive reads; an out-of-range window
            // escalates before it raises); see `ForeignJniEntry::enter_leaf`.
            let _fx = ForeignJniEntry::enter_leaf(array);
            if array == 0 || buf.is_null() || start < 0 || len < 0 {
                return;
            }
            with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                // JNI contract: validate the requested window against the array
                // length BEFORE touching memory; raise AIOOBE on a bad range.
                if !region_bounds_ok(start, len, shared.mem.heap.array_length(oref)) {
                    return None;
                }
                for i in 0..len as usize {
                    let val = shared
                        .mem
                        .heap
                        .get_array_element(oref, start as usize + i)
                        .ok()?;
                    let elem = match val {
                        Value::$value_variant(v) => v as $rust_type,
                        _ => $default,
                    };
                    unsafe {
                        *buf.add(i) = elem;
                    }
                }
                Some(())
            });
        }
    };
}

get_array_region!(jni_get_boolean_array_region, JBoolean, Int, 0); // 199
get_array_region!(jni_get_byte_array_region, JByte, Int, 0); // 200
get_array_region!(jni_get_char_array_region, JChar, Int, 0); // 201
get_array_region!(jni_get_short_array_region, JShort, Int, 0); // 202
get_array_region!(jni_get_int_array_region, JInt, Int, 0); // 203
get_array_region!(jni_get_long_array_region, JLong, Long, 0); // 204
get_array_region!(jni_get_float_array_region, JFloat, Float, 0.0); // 205
get_array_region!(jni_get_double_array_region, JDouble, Double, 0.0); // 206

// ---- Indices 207-214: Set<Type>ArrayRegion ----
macro_rules! set_array_region {
    ($name:ident, $rust_type:ty, $value_constructor:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            start: JSize,
            len: JSize,
            buf: *const $rust_type,
        ) {
            // gcd d5/f: a leaf (primitive stores, which need no barrier; an
            // out-of-range window escalates before it raises); see
            // `ForeignJniEntry::enter_leaf`.
            let _fx = ForeignJniEntry::enter_leaf(array);
            if array == 0 || buf.is_null() || start < 0 || len < 0 {
                return;
            }
            with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                // JNI contract: validate the requested window against the array
                // length BEFORE writing; raise AIOOBE on a bad range so no
                // partial / out-of-range store is performed.
                if !region_bounds_ok(start, len, shared.mem.heap.array_length(oref)) {
                    return None;
                }
                for i in 0..len as usize {
                    let val = unsafe { *buf.add(i) };
                    let _ = shared.mem.heap.set_array_element(
                        oref,
                        start as usize + i,
                        $value_constructor(val),
                    );
                }
                Some(())
            });
        }
    };
}

set_array_region!(jni_set_boolean_array_region, JBoolean, |v: JBoolean| {
    Value::Int(v as i32)
}); // 207
set_array_region!(jni_set_byte_array_region, JByte, |v: JByte| Value::Int(
    v as i32
)); // 208
set_array_region!(jni_set_char_array_region, JChar, |v: JChar| Value::Int(
    v as i32
)); // 209
set_array_region!(jni_set_short_array_region, JShort, |v: JShort| Value::Int(
    v as i32
)); // 210
set_array_region!(jni_set_int_array_region, JInt, |v: JInt| Value::Int(v)); // 211

// Index 212: SetLongArrayRegion — hand-unrolled (vs the macro) to add the
// long-smuggle mint chokepoint: native code bulk-storing raw jobject handles
// into a long[] (see smuggled_longs). Strict object-start probe per element,
// so ordinary numeric bulk stores register nothing.
extern "C" fn jni_set_long_array_region(
    _env: JNIEnv,
    array: JArray,
    start: JSize,
    len: JSize,
    buf: *const JLong,
) {
    let _fx = ForeignJniEntry::enter();
    if array == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        // JNI contract: validate the requested window against the array
        // length BEFORE writing; raise AIOOBE on a bad range so no
        // partial / out-of-range store is performed.
        if !region_bounds_ok(start, len, shared.mem.heap.array_length(oref)) {
            return None;
        }
        for i in 0..len as usize {
            let val = unsafe { *buf.add(i) };
            let bits = val as u64;
            if bits != 0
                && bits & 0x7 == 0
                && shared.mem.heap.is_object_address(bits as usize).is_some()
            {
                crate::memory::smuggled_longs::record_minted_long(&shared.mem.heap, bits);
            }
            let _ = shared
                .mem
                .heap
                .set_array_element(oref, start as usize + i, Value::Long(val));
        }
        Some(())
    });
}
set_array_region!(
    jni_set_float_array_region,
    JFloat,
    |v: JFloat| Value::Float(v)
); // 213
set_array_region!(jni_set_double_array_region, JDouble, |v: JDouble| {
    Value::Double(v)
}); // 214

// ---- Index 217: MonitorEnter ----
//
// gc-common w2-c (2026-09-23), `common-a-jni-placeholder-thread-id-at-barrier`:
// this used `ThreadId(0)` — the MAIN thread's id — as both the monitor owner and
// the barrier identity, whatever thread it ran on. Consequences, all fixed here:
//
// * ownership: every JNI caller shared main's identity, so two native threads
//   could both "hold" one monitor (a re-entrant acquire), and a native
//   `MonitorEnter` on an object its own Java caller already locked with
//   `synchronized` contended with ITSELF (unless it was main) and never woke;
// * the barrier: the contended path bumped only the anonymous blocked counter
//   and never raised `in_blocked_region`, so the production identity census
//   still counted the thread while it slept in `block_enter` — a pause that
//   started then waited for an arrival that could not come, and the
//   `BlockedGuard` drop on acquisition then waited for that pause (deadlock;
//   the take-over cannot excuse it, it is not in compiled code). A pre-STW
//   arrival under `ThreadId(0)` was either skipped (main is the initiator) or
//   filled main's quota slot.
//
// Now: a thread with a JNI binding that is a counted mutator takes the
// canonical GC-safe contended acquire every Java `monitorenter` uses
// (`monitor_enter_blocking`: pin, retire TLAB, deposit roots + raise the flag,
// block, remap); one already excluded from the census (an idle foreign-attached
// thread, a `host_thread_enter_native` thread) acquires without joining the
// census, its mark-word operation run with no pause in progress
// (`jni_monitor_enter_excluded`); an unbound caller is resolved by OS tid and
// never arrives under a guessed identity, and one the registry cannot name
// locks under an id of its own (`unnamed_caller_monitor_owner`).
extern "C" fn jni_monitor_enter(_env: JNIEnv, obj: JObject) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return JNI_ERR;
    }
    if jni_bound_thread_id().is_some() {
        return with_jni_context(|shared, thread| {
            let oref = jobject_to_obj(obj)?;
            jni_monitor_enter_as(shared, thread, obj, oref)?;
            Some(JNI_OK)
        })
        .flatten()
        .unwrap_or(JNI_ERR);
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        // No JNI binding (a registered thread whose binding a nested native
        // cleared, or a host thread). A registered thread running on this OS
        // thread still has its `JvmThread`: take the bound path with it, so a
        // pause that starts while it sleeps excludes it by identity
        // (gc-common w4-g, `common-w3a-jni-unbound-counted-monitor-enter` (1)).
        if let Some(thread) = unbound_caller_jvm_thread(shared) {
            // SAFETY: `unbound_caller_jvm_thread` answers only the published
            // `JvmThread` of the one alive registry entry whose OS tid is THIS
            // thread's, i.e. the thread executing this call; it cannot be
            // dropped while its own thread runs. The `&mut` has the same
            // aliasing profile as `with_jni_context`'s (the interpreter up
            // the stack holds the thread across the native call).
            jni_monitor_enter_as(shared, unsafe { &mut *thread }, obj, oref)?;
            return Some(JNI_OK);
        }
        // Otherwise: resolve the caller by its OS tid alone.
        let me = shared
            .threads
            .thread_registry
            .thread_id_for_current_os_tid();
        let excluded = me
            .and_then(|t| shared.threads.thread_registry.gc_block_state_of(t))
            .map(|s| {
                s.in_blocked_region
                    .load(std::sync::atomic::Ordering::Acquire)
            })
            .unwrap_or(false);
        // An unnamed caller (`me == None`) locks under an id of its own, never
        // `ThreadId(0)`: that is `main`'s real id, so a `MonitorEnter` on an
        // object `main` held thin was taken as `main`'s re-entry (two threads
        // inside one monitor) -- `r11w7-lock-jni-unresolved-caller-locks-as-main`.
        let owner = match me {
            Some(t) => t,
            None => unnamed_caller_monitor_owner(shared, true)?,
        };
        if excluded {
            jni_monitor_enter_excluded(shared, obj, oref, owner)?;
        } else if let Some(m) = shared.threads.monitors.enter_or_contend(oref, owner) {
            // Counted (or unknown) and without a `JvmThread` to deposit from.
            // A counted thread arrives under its REAL id; an unregistered one
            // (`me == None`) does not arrive at all — see
            // `jni_caller_thread_id` for why a placeholder is worse than a
            // hang. Reached now only by a caller with no published
            // `JvmThread` (see `unbound_caller_jvm_thread`; a mounted virtual
            // thread is resolved there since w6-g).
            let blk = shared.mem.gc_barrier.enter_blocked();
            if blk.pre_stw {
                if let Some(tid) = me {
                    let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
                }
            }
            m.block_enter(owner);
            drop(blk);
        }
        Some(JNI_OK)
    })
    .flatten()
    .unwrap_or(JNI_ERR)
}

/// `MonitorEnter` for a caller whose `JvmThread` is known: an excluded
/// (blocked-region) thread acquires without joining the census, a counted one
/// takes the canonical GC-safe contended acquire. `None`: `obj` (a global ref)
/// no longer resolves.
fn jni_monitor_enter_as(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: JObject,
    oref: ObjectRef,
) -> Option<()> {
    if thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
    {
        // Already excluded from every census (and its roots are in its
        // deposited snapshot). Running the blocking protocol here would
        // CLEAR the flag on the way out (`check_post_block_gc`) and turn
        // an idle thread back into a counted mutator that is parked in
        // host code — the next pause would wait for it forever.
        jni_monitor_enter_excluded(shared, obj, oref, thread.thread_id)
    } else {
        let _ = crate::vm::vm_exec::monitor_enter_blocking(shared, thread, oref);
        Some(())
    }
}

/// The `JvmThread` of an UNBOUND JNI caller that is a registered thread on
/// this OS thread, if exactly one alive entry claims this OS thread and it has
/// published its `JvmThread` address.
///
/// gc-common w4-g (`common-w3a-jni-unbound-counted-monitor-enter`, residual
/// 1): a registered, COUNTED thread whose JNI binding a nested native cleared
/// used to take the anonymous contended path in `MonitorEnter` -- no deposit,
/// no `in_blocked_region` -- so a pause that STARTED while it slept in
/// `block_enter` still counted it and waited until the monitor was granted (a
/// hang if the holder was parked at that pause). Its `JvmThread` is on this
/// very OS thread, published in the registry (`set_jvm_thread_addr`) for the
/// take-over; with it the caller takes the same GC-safe acquire a bound
/// caller does. `None` for an unregistered host thread and for a thread that
/// has not published its address.
///
/// gc-common w6-g (residual 2): an unbound MOUNTED virtual thread is resolved
/// too. Its carrier's OS tid is also claimed by every parked virtual thread
/// that last ran there; `thread_id_for_current_os_tid` now answers the
/// claimant with the newest publish on this OS thread, which is the mounted
/// one (a mount publishes before it runs Java), and its address is read by id
/// (`own_jvm_thread_addr`), so no parked thread's `JvmThread` is ever touched.
fn unbound_caller_jvm_thread(shared: &SharedVm) -> Option<*mut JvmThread> {
    let registry = &shared.threads.thread_registry;
    let me = registry.thread_id_for_current_os_tid()?;
    let thread = registry.own_jvm_thread_addr(me)? as *mut JvmThread;
    // SAFETY: `me` is the identity running on this OS thread (see above), so
    // its published, non-zero `jvm_thread_addr` points at the live `JvmThread`
    // executing this call; `thread_id` is an immutable `Copy` field.
    // Cross-checked so a stale publication of another id can never be adopted.
    (unsafe { (*thread).thread_id } == me).then_some(thread)
}

/// The calling OS thread's own `JvmThread` in `shared`, for a C JVMTI function
/// that asks about the current thread (interpreter round i1 wave 22, lane L1:
/// `GetCurrentThread`, `GetFrameCount`, `GetStackTrace`, `GetFrameLocation`,
/// `jvmti::native_env`): the one the JNI layer installed on this thread (a
/// native call, an event callback, an attached thread) when `shared`'s
/// registry publishes it under its own id, else the registered thread this OS
/// thread runs ([`unbound_caller_jvm_thread`]). `None` for a thread `shared`
/// does not know. Only the calling thread may dereference the answer, and
/// only for the duration of the call it is in.
pub(crate) fn current_jvm_thread_of(shared: &SharedVm) -> Option<*mut JvmThread> {
    let installed = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut()) as *mut JvmThread;
    if !installed.is_null() {
        // SAFETY: `JNI_THREAD` holds a pointer to the live `JvmThread` running
        // this call (see `set_jni_thread`); `thread_id` is an immutable `Copy`
        // field.
        let tid = unsafe { (*installed).thread_id };
        // Cast: the pointer's address, as the registry publishes it.
        if shared.threads.thread_registry.own_jvm_thread_addr(tid) == Some(installed as usize) {
            return Some(installed);
        }
    }
    unbound_caller_jvm_thread(shared)
}

/// Acquire `obj` (resolved to `oref`) for a caller the STW census already
/// excludes (`in_blocked_region` raised). It never joins a census: no pause is
/// waiting for this thread and its roots are in its deposited snapshot.
///
/// The mark-word half runs inside `GcBarrier::with_no_pause_in_progress`
/// (`r11w4-sync-jni-excluded-monitor-ops-touch-a-movable-header`). Because
/// no pause waits for this thread, one could otherwise relocate the object
/// between the resolution and the CAS (or during an inflation), so the lock
/// would land on the vacated copy. Waiting a pause out is safe exactly
/// because this thread is excluded: no pause can be waiting for it. The
/// header operation does not spin while it holds the barrier lock. The
/// contended wait (`block_enter`, which touches no header) stays outside the
/// scope, as it always did.
///
/// `None`: `obj` is a global ref that no longer resolves.
fn jni_monitor_enter_excluded(
    shared: &SharedVm,
    obj: JObject,
    oref: ObjectRef,
    owner: ThreadId,
) -> Option<()> {
    let contended = shared.mem.gc_barrier.with_no_pause_in_progress(|| {
        let oref = excluded_caller_current_ref(obj, oref)?;
        Some(
            shared
                .threads
                .monitors
                .enter_or_contend_without_spin(oref, owner),
        )
    })?;
    if let Some(m) = contended {
        m.block_enter(owner);
    }
    Some(())
}

/// What `obj` names NOW, for a census-excluded caller inside
/// `GcBarrier::with_no_pause_in_progress`. The caller resolved it to
/// `resolved` before the scope, and a collection may have run since. A
/// global ref is resolved again through its table, which that collection
/// remapped. A local ref is a raw address either way (nothing rewrites the
/// native's copy: `docs/known-issues/gc/common-w2c-jni-local-refs-are-raw-addresses.md`),
/// so it keeps its earlier validation. That validation takes heap locks,
/// which must not be taken under the barrier lock.
fn excluded_caller_current_ref(obj: JObject, resolved: ObjectRef) -> Option<ObjectRef> {
    // A `jclass` (an odd `ClassId` has bit 0 set) is NOT re-resolved here: its
    // decode takes the mirror-cache and class-manager locks, and a thread that
    // mints a mirror holds the cache's write lock across an allocation that
    // may wait on this barrier. It keeps its earlier resolution, like a local.
    if obj & 1 == 1 && !is_jclass_handle(obj) {
        jobject_to_obj(obj)
    } else {
        Some(resolved)
    }
}

/// Whether the JNI caller `me` is excluded from the STW census
/// (`in_blocked_region` raised): its bound `JvmThread`'s flag if a binding is
/// installed (what `MonitorEnter` tests), else its registry entry's.
fn jni_caller_is_census_excluded(shared: &SharedVm, me: ThreadId) -> bool {
    use std::sync::atomic::Ordering;
    let bound = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    if !bound.is_null() {
        // SAFETY: as in `jni_bound_thread_id`: a non-null `JNI_THREAD` points at
        // a live `JvmThread` while installed. Only an atomic is read, through a
        // shared reference.
        let thread = unsafe { &*(bound as *const JvmThread) };
        return thread
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire);
    }
    shared
        .threads
        .thread_registry
        .gc_block_state_of(me)
        .is_some_and(|s| s.in_blocked_region.load(Ordering::Acquire))
}

thread_local! {
    /// This OS thread's monitor-owner id for JNI `MonitorEnter` /
    /// `MonitorExit` when the registry cannot name it, with the address of the
    /// `SharedVm` that issued it. See [`unnamed_caller_monitor_owner`].
    static JNI_UNNAMED_MONITOR_OWNER: Cell<Option<(usize, ThreadId)>> =
        const { Cell::new(None) };
}

/// The monitor owner for a JNI caller with no JNI binding whose OS thread the
/// registry cannot name: an unregistered host thread, a mounted virtual thread
/// sharing its carrier's OS tid, or any unbound caller on a platform with no
/// OS-tid backend.
///
/// Such a caller used `ThreadId(0)`, which is `main`'s real id: its enter on
/// an object `main` held thin became `main`'s re-entry, and its exit could
/// release `main`'s lock (`r11w7-lock-jni-unresolved-caller-locks-as-main`).
/// It now gets an id from the registry's own counter: never a registered
/// thread's id, below `u32::MAX` (so the lock stays in the mark word and
/// excludes Java threads), stable for this OS thread (so its `MonitorExit`
/// matches its `MonitorEnter`), and different for every other thread. The id
/// is NOT registered. It arrives at no barrier (as before) and publishes
/// nothing to JMX.
///
/// `issue == false` (`MonitorExit`): answer only an id this thread already
/// holds. `None` then means this thread never locked anything under one, so
/// there is nothing of its own to release. The cached id is tied to the
/// `SharedVm` that issued it (by `vm_identity`, which is never reused; it was
/// the VM's address until interpreter round i1 wave 23, which a later VM at
/// the same address inherited), and is dropped if that VM has since given it
/// to a live registered thread.
fn unnamed_caller_monitor_owner(shared: &SharedVm, issue: bool) -> Option<ThreadId> {
    let vm_key = shared.vm_identity;
    let registry = &shared.threads.thread_registry;
    JNI_UNNAMED_MONITOR_OWNER
        .try_with(|c| match c.get() {
            Some((key, tid)) if key == vm_key && !registry.is_alive(tid) => Some(tid),
            _ if issue => {
                let tid = registry.next_thread_id();
                c.set(Some((vm_key, tid)));
                Some(tid)
            }
            _ => None,
        })
        .ok()
        .flatten()
}

// ---- Index 218: MonitorExit ----
extern "C" fn jni_monitor_exit(_env: JNIEnv, obj: JObject) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return JNI_ERR;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        // The SAME identity `jni_monitor_enter` acquired under (ownership is
        // keyed by it). `monitor_exit_and_retract_jmx` also retracts the JMX
        // ownership publish `monitor_enter_blocking` made; for a caller that
        // never published it the retract is a no-op.
        match jni_caller_thread_id(shared) {
            // A census-excluded caller releases with no pause in progress, for
            // the reason `jni_monitor_enter_excluded` acquires that way: the
            // release CASes (or, inflated, reads) the mark word.
            Some(me) if jni_caller_is_census_excluded(shared, me) => {
                shared.mem.gc_barrier.with_no_pause_in_progress(|| {
                    let oref = excluded_caller_current_ref(obj, oref)?;
                    let _ = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, oref, me);
                    Some(())
                })?;
            }
            Some(me) => {
                let _ = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, oref, me);
            }
            // The id `jni_monitor_enter` locked under, never `main`'s
            // `ThreadId(0)`. No id: this thread holds nothing of its own.
            None => {
                if let Some(owner) = unnamed_caller_monitor_owner(shared, false) {
                    let _ = shared.threads.monitors.exit(oref, owner);
                }
            }
        }
        Some(JNI_OK)
    })
    .flatten()
    .unwrap_or(JNI_ERR)
}

// ---- Index 219: GetJavaVM ----
extern "C" fn jni_get_java_vm(_env: JNIEnv, vm: *mut JavaVM) -> JInt {
    if vm.is_null() {
        return JNI_ERR;
    }
    init_jni_table();
    // `AtomicPtr<usize>` has the same layout as `*mut usize`, so the address of
    // the atomic itself is a valid pointer to the invoke-table pointer.
    unsafe {
        *vm = std::ptr::addr_of!(JNI_INVOKE_TABLE_PTR).cast();
    }
    JNI_OK
}

// ---- Index 228: ExceptionCheck ----
extern "C" fn jni_exception_check(_env: JNIEnv) -> JBoolean {
    // This returned `JNI_FALSE` unconditionally — a constant, not a check.
    //
    // `if ((*env)->ExceptionCheck(env)) { … }` is how essentially every JNI
    // library asks "did that up-call throw?", so a hard `false` did not fail
    // loudly: it made every error-handling branch in every native library
    // unreachable, and each caller went on to use a return value the JNI
    // spec leaves undefined once an exception is pending.
    //
    // The pending slot is the authority here rather than
    // `native_pending_return`, because the slot is what `ExceptionClear`
    // resets — a check that consulted the root instead would keep answering
    // `true` after the native had cleared.
    if JNI_PENDING_EXCEPTION.with(|cell| cell.get()) != 0 {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

// ---- Index 5: DefineClass ----
extern "C" fn jni_define_class(
    _env: JNIEnv,
    name: *const c_char,
    loader: JObject,
    buf: *const u8,
    len: JSize,
) -> JClass {
    let _fx = ForeignJniEntry::enter();
    // Two policies, chosen per VM (round 12 wave 6, lane jni;
    // `r12w5-tier3-jni-defineclass-ignores-the-loader-FIXED-20260926.md`):
    //
    // * `--jdk-only`: the define goes through `ClassLoader.defineClass1`'s
    //   registered native with the caller's `loader`, the backend every Java
    //   define uses ([`jni_define_class_through_loader`]): the loader's
    //   namespace, the defining-loader record, the transform chain, the
    //   metaspace check, and HotSpot's exception types.
    // * `--compatible` (and the kill switch): the legacy define below,
    //   byte-for-byte: every class lands in the application namespace and
    //   every failure is a `NoClassDefFoundError`.
    let through_loader = jni_define_class_through_loader_active();
    // Per JNI, `name` may be NULL (the name is then taken from the class file);
    // when supplied it is the expected binary name. The bytecode buffer is
    // mandatory: a NULL/empty/negative-length buffer is a hard error.
    if buf.is_null() || len <= 0 {
        if through_loader {
            // HotSpot parses the empty stream and fails with this.
            raise_jni_throwable("java/lang/ClassFormatError", "Truncated class file");
        } else {
            raise_jni_no_class_def_found(
                "DefineClass called with a null or empty bytecode buffer",
            );
        }
        return 0;
    }
    // The class name may be NULL (JNI allows deriving it from the class file).
    let class_name = unsafe { cstr_to_str(name) }.map(|s| s.replace('.', "/"));

    // SAFETY: the caller guarantees `buf` points to `len` readable bytes for
    // the duration of the call (standard JNI DefineClass contract). We copy the
    // bytes out immediately so the slice does not outlive this borrow.
    let bytes: Vec<u8> = unsafe { std::slice::from_raw_parts(buf, len as usize) }.to_vec();

    if through_loader {
        // `None`: no attached `JvmThread` to run the define on; the legacy
        // define below is all such a caller could ever get.
        if let Some(defined) = jni_define_class_through_loader(class_name.as_deref(), loader, &bytes)
        {
            return defined;
        }
    }

    // Legacy define (`--compatible`): define the class from the caller-supplied
    // `buf[..len]` bytecode via the same `define_class` path the interpreter
    // uses for `ClassLoader.defineClass` / agent retransform, so JNI/agent code
    // that synthesises classes at runtime gets the bytes it actually passed —
    // not a same-named class loaded from the classpath.
    let result = with_shared_vm(|shared| {
        // If the caller did not supply a name, the class manager will derive it
        // from the class file's `this_class` entry; use the empty string as a
        // placeholder that `define_class` overrides from the bytes.
        let define_name = class_name.as_deref().unwrap_or("");
        let mut cm = shared.classes.class_manager_write();
        let next_id_before = cm.class_store.next_id();
        let cid = cm
            .define_class(
                define_name,
                &bytes,
                cratonvm_types::ClassLoaderId::Application,
            )
            .ok()?;
        drop(cm);
        // Mirror the interpreter's defineClass path: invalidate any JIT code
        // that may have inlined from a previously-loaded class of this name so
        // a redefinition is honoured rather than served stale. Only a define
        // that can have replaced one does (round 12 wave 5,
        // `define_withdraws_by_name`).
        let withdraws =
            crate::vm::realms::jit_realm::define_withdraws_by_name(cid, next_id_before, false);
        if let Some(n) = class_name.as_deref().filter(|_| withdraws) {
            let _ = shared.jit.jit_cache.write().invalidate_for_class(n);
            // r10 lane `tier2`: the profile store is keyed by (class_id, method,
            // descriptor) and `class_id` is STABLE across a redefine, so without this the
            // new bytecode's execution blends into branch ratios and receiver tables
            // gathered from the OLD bytecode. The unload path in `memory/gc.rs` has always
            // invalidated both; redefine invalidated only the tiering verdicts.
            shared.jit.profile_store.invalidate_class(cid.as_u32());
            shared.jit.tiered_manager.on_class_redefined(cid, n);
        }
        Some(class_id_to_jclass(cid))
    })
    .flatten();

    match result {
        Some(c) => c,
        None => {
            // Defining from the supplied bytes failed (malformed class file,
            // linkage error, or no VM context). Surface it as a real Java
            // exception instead of silently substituting a classpath class.
            let label = class_name.as_deref().unwrap_or("<unnamed>");
            raise_jni_no_class_def_found(&format!(
                "DefineClass failed to define class {label} from the supplied bytecode"
            ));
            0
        }
    }
}

/// Does this VM's `DefineClass` go through the caller's loader
/// ([`jni_define_class_through_loader`])? In both modes: the owner turned it
/// on for `--compatible` too (2026-09-27); the kill switch below gives either
/// mode the legacy define back.
fn jni_define_class_through_loader_active() -> bool {
    jni_define_class_loader_switch()
}

/// `CRATONVM_JNI_DEFINECLASS_LOADER`, latched like every flag. Default ON;
/// `0` / `false` / `off` / `no` gives `--jdk-only` the legacy define too.
fn jni_define_class_loader_switch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JNI_DEFINECLASS_LOADER")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("0" | "false" | "off" | "no")
        )
    })
}

/// What [`jni_define_class_through_loader`]'s up-call came back with.
enum JniDefineOutcome {
    Defined(ClassId),
    /// A Java argument could not be allocated; `OutOfMemoryError` is raised
    /// outside the thread borrow (`raise_jni_oom` takes it itself).
    OutOfMemory(&'static str),
    /// The define failed; whatever it threw is already pending.
    Failed,
}

/// JNI `DefineClass` as HotSpot's `jni_DefineClass` does it: the class is
/// defined BY `loader` (null = the bootstrap loader, which the native models
/// as namespace 0 as it does for every Java caller passing null). The define
/// is `ClassLoader.defineClass1(loader, name, bytes, 0, len, null, null)` run
/// through the registered native, so it is exactly the backend
/// `ClassLoader.defineClass` uses: the loader's namespace
/// (`loader_namespace_id`), its supertypes linked through that loader, the
/// defining-loader record and the mirror's `classLoader` field, the
/// transform-on-load chain, the metaspace limit, the JIT withdrawal
/// (`define_withdraws_by_name`), and typed failures -- `ClassFormatError`,
/// `NoClassDefFoundError` for a name mismatch, `LinkageError` for a duplicate
/// in the same loader, `SecurityException` for `java.*`.
///
/// `None` when this OS thread has no attached `JvmThread` to run it on.
fn jni_define_class_through_loader(
    class_name: Option<&str>,
    loader: JObject,
    bytes: &[u8],
) -> Option<JClass> {
    const DEFINE_CLASS1_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;";
    // A class file longer than an `int` cannot be a `byte[]`.
    let len = i32::try_from(bytes.len()).ok()?;
    let _fg = ForeignCallGuard::enter();
    let outcome = with_jni_context(|shared, thread| {
        let Ok(class_loader) = shared.load_class_concurrent("java/lang/ClassLoader") else {
            return JniDefineOutcome::Failed;
        };
        // Both arguments are allocated without a collection (JNI's rule: the
        // native holds raw references), and the up-call roots its arguments.
        let name_arg = match class_name {
            Some(n) => match crate::vm::try_create_java_string_uninterned(shared, n) {
                Some(s) => Value::Object(Some(s)),
                None => return JniDefineOutcome::OutOfMemory("a java/lang/String"),
            },
            None => Value::Object(None),
        };
        let Some(array) =
            shared
                .mem
                .heap
                .try_alloc_array_full(ClassId::new(0), ArrayElementType::Byte, bytes.len())
        else {
            return JniDefineOutcome::OutOfMemory("a byte array");
        };
        for (i, &b) in bytes.iter().enumerate() {
            let _ = shared
                .mem
                .heap
                .set_array_element(array, i, Value::Int(b as i8 as i32));
        }
        // Decoded last, after the allocations, as every JNI function decodes
        // its handles before it touches the heap.
        let loader_arg = Value::Object(if loader == 0 {
            None
        } else {
            jobject_to_obj(loader)
        });
        let args = [
            loader_arg,
            name_arg,
            Value::Object(Some(array)),
            Value::Int(0),
            Value::Int(len),
            Value::Object(None),
            Value::Object(None),
        ];
        // `no_retarget`: `defineClass1` is static; its first argument is the
        // loader, not a receiver to dispatch on.
        let result = crate::vm::invoke_on_class_shared_no_retarget(
            shared,
            thread,
            class_loader,
            "defineClass1",
            DEFINE_CLASS1_DESC,
            &args,
        );
        match result {
            Ok(Some(Value::Object(Some(mirror)))) => {
                match crate::vm::class_id_from_mirror(shared, mirror) {
                    Some(cid) => JniDefineOutcome::Defined(cid),
                    None => JniDefineOutcome::Failed,
                }
            }
            Ok(_) => JniDefineOutcome::Failed,
            Err(err) => {
                // The native answers typed VM errors; JNI hands the native a
                // pending throwable, as the interpreter's invoke arms do
                // (`classify_fastpath_invoke_error`). Converted here whatever
                // the up-call switch says: this path is `--jdk-only`'s own.
                let err = jni_vm_error_as_throwable(shared, thread, err);
                let _ = jni_surface_jdk_only(shared, thread, Err(err));
                JniDefineOutcome::Failed
            }
        }
    })?;
    Some(match outcome {
        JniDefineOutcome::Defined(cid) => class_id_to_jclass(cid),
        JniDefineOutcome::OutOfMemory(what) => {
            let _ = with_shared_vm(|shared| raise_jni_oom(shared, what));
            0
        }
        JniDefineOutcome::Failed => {
            // Never a bare NULL: an error the native could not type (an
            // internal VM failure) still leaves something pending.
            if JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                let label = class_name.unwrap_or("<unnamed>");
                raise_jni_no_class_def_found(&format!(
                    "DefineClass failed to define class {label} from the supplied bytecode"
                ));
            }
            0
        }
    })
}

/// Allocate a JNI-requested object, or raise `OutOfMemoryError` and return
/// `None` -- JNI's contract for an allocating function that fails is `NULL`
/// with a pending `OutOfMemoryError`.
///
/// gc-common w5-g (`handoff-w4c-infallible-callers-residue` §2): these sites
/// used the infallible `alloc_object` / `alloc_array`, which ABORT the process
/// on G1 when the heap is exhausted (`FATAL: G1 infallible alloc_object`), so
/// a native library asking for one array too many killed the VM instead of
/// seeing a catchable error. The fallible twins take the same no-collection
/// path, so a successful allocation is unchanged. No collection is attempted
/// here: native code holds raw local references across the call.
fn jni_alloc_or_oom(
    shared: &SharedVm,
    what: &str,
    alloc: impl FnOnce(&crate::memory::vm_heap::VmHeap) -> Option<ObjectRef>,
) -> Option<ObjectRef> {
    let obj = alloc(&shared.mem.heap);
    if obj.is_none() {
        raise_jni_oom(shared, what);
    }
    obj
}

/// Set a pending `OutOfMemoryError` for the current JNI call. Built without a
/// collection (the allocation that failed already took the no-GC path, and a
/// native frame holds raw references); if even that cannot be allocated, the
/// VM's preallocated OOME is thrown, and failing that the sentinel is set so
/// the failure is never silently a plain `NULL`.
fn raise_jni_oom(shared: &SharedVm, what: &str) {
    let msg = format!("Java heap space: failed to allocate {what} in JNI");
    let raised = with_jni_context(|ctx_shared, thread| {
        match crate::runtime::exceptions::create_exception_object_no_collect(
            ctx_shared,
            thread,
            "java/lang/OutOfMemoryError",
            Some(&msg),
        ) {
            Ok(exc) => {
                set_jni_pending_exception_object(exc);
                true
            }
            Err(_) => false,
        }
    })
    .unwrap_or(false);
    if raised {
        return;
    }
    let singleton = *shared.mem.singleton_oom.read();
    match singleton {
        Some(oom) => set_jni_pending_exception_object(oom),
        None => JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX)),
    }
}

/// Raise a `NoClassDefFoundError` on the current thread so a failed
/// `DefineClass` surfaces as a real Java exception rather than a fabricated
/// null/0 return. Mirrors [`raise_jni_aioobe`].
fn raise_jni_no_class_def_found(msg: &str) {
    let raised =
        with_jni_context(
            |shared, thread| match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/NoClassDefFoundError",
                Some(msg),
            ) {
                Ok(exc) => {
                    set_jni_pending_exception_object(exc);
                    Ok(())
                }
                Err(e) => Err(is_heap_exhaustion(&e)),
            },
        )
        .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        // No thread context or allocation failed: see `pend_unbuilt_throwable`.
        pend_unbuilt_throwable(heap_exhausted);
    }
}

// ---- Index 7: FromReflectedMethod ----
// Convert a java.lang.reflect.Method/Constructor to a JMethodID.
extern "C" fn jni_from_reflected_method(_env: JNIEnv, method: JObject) -> JMethodID {
    let _fx = ForeignJniEntry::enter();
    if method == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(method)?;
        // Reflected method objects store class_id and method_index in fields 0 and 1.
        let class_id_val = match shared.mem.heap.get_field(oref, 0) {
            Value::Int(i) => i as u32,
            _ => return None,
        };
        let method_idx = match shared.mem.heap.get_field(oref, 1) {
            Value::Int(i) => i as u16,
            _ => return None,
        };
        Some(encode_method_id(ClassId::new(class_id_val), method_idx))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 8: FromReflectedField ----
// Convert a java.lang.reflect.Field to a JFieldID.
extern "C" fn jni_from_reflected_field(_env: JNIEnv, field: JObject) -> JFieldID {
    let _fx = ForeignJniEntry::enter();
    if field == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(field)?;
        // Reflected field objects store class_id in field 0 and field_index in field 1.
        let class_id_val = match shared.mem.heap.get_field(oref, 0) {
            Value::Int(i) => i as u32,
            _ => return None,
        };
        let field_idx = match shared.mem.heap.get_field(oref, 1) {
            Value::Int(i) => i as usize,
            _ => return None,
        };
        Some(encode_field_id(ClassId::new(class_id_val), field_idx))
    })
    .flatten()
    .unwrap_or(0)
}

/// Resolve a class a JNI entry point needs to allocate against, refusing rather
/// than fabricating under `--jdk-only`.
///
/// The "refuse diagnosably" shape of `vm_init::ensure_bootstrap_compat_class`,
/// adapted to a caller with no Java-side error channel: a JNI function cannot
/// throw from here, and its documented failure value is NULL, so a refusal
/// returns `None` and the caller returns null after this has warned and named
/// the class. The real class is preferred first, exactly as before, so on any
/// complete image nothing about this path changes.
///
/// Added 2026-08-10 with JDK-only wave 2 step 3, which deleted the infallible
/// `ensure_synthetic_class` these three sites used to reach.
fn jni_class_or_refuse(shared: &SharedVm, name: &str, num_fields: usize) -> Option<ClassId> {
    if let Ok(id) = shared.load_class_concurrent(name) {
        return Some(id);
    }
    match shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(name, num_fields)
    {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::warn!(
                class = name,
                error = %err,
                "--jdk-only: refusing to fabricate this class for a JNI entry point. The \
                 call returns NULL, which is JNI's documented failure value, and the \
                 caller sees it at its own call site."
            );
            None
        }
    }
}

// ---- Index 9: ToReflectedMethod ----
// Convert a JMethodID to a java.lang.reflect.Method object.
extern "C" fn jni_to_reflected_method(
    _env: JNIEnv,
    clazz: JClass,
    method_id: JMethodID,
    _is_static: JBoolean,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if method_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, method_index) = decode_method_id(method_id);
        let class_id = if clazz != 0 {
            jclass_class_id(clazz)
        } else {
            decl_class_id
        };
        // Allocate a synthetic Method object with class_id and method_index in fields.
        // Resolve the real `java.lang.reflect.Method` class — `find_class_by_name`
        // only sees already-loaded classes, so `load_class_concurrent` is used to
        // force the load. Allocating with `ClassId::new(0)` (`java/lang/Object`,
        // zero declared fields) but 4 slots produces an undersized object the
        // GC's `get_field` bounds guard rejects.
        let Some(method_class_id) = jni_class_or_refuse(shared, "java/lang/reflect/Method", 4)
        else {
            return 0;
        };
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .get_class(method_class_id)
            .map_or(4, |c| c.num_total_fields.max(4));
        let Some(obj) = jni_alloc_or_oom(shared, "a java.lang.reflect.Method", |heap| {
            heap.try_alloc_object_full(method_class_id, num_fields)
        }) else {
            return 0;
        };
        shared
            .mem
            .heap
            .set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared
            .mem
            .heap
            .set_field(obj, 1, Value::Int(method_index as i32));
        new_local_handle(obj)
    })
    .unwrap_or(0)
}

// ---- Index 12: ToReflectedField ----
// Convert a JFieldID to a java.lang.reflect.Field object.
extern "C" fn jni_to_reflected_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
    _is_static: JBoolean,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let class_id = if clazz != 0 {
            jclass_class_id(clazz)
        } else {
            decl_class_id
        };
        // Resolve the real `java.lang.reflect.Field` class (see the matching
        // comment in `jni_to_reflected_method`): allocating with
        // `ClassId::new(0)` + 4 slots produces an undersized object the GC's
        // `get_field` bounds guard rejects.
        let Some(field_class_id) = jni_class_or_refuse(shared, "java/lang/reflect/Field", 4) else {
            return 0;
        };
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .get_class(field_class_id)
            .map_or(4, |c| c.num_total_fields.max(4));
        let Some(obj) = jni_alloc_or_oom(shared, "a java.lang.reflect.Field", |heap| {
            heap.try_alloc_object_full(field_class_id, num_fields)
        }) else {
            return 0;
        };
        shared
            .mem
            .heap
            .set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared
            .mem
            .heap
            .set_field(obj, 1, Value::Int(field_index as i32));
        new_local_handle(obj)
    })
    .unwrap_or(0)
}

// ---- Index 220: GetStringRegion ----
extern "C" fn jni_get_string_region(
    _env: JNIEnv,
    str_obj: JString,
    start: JSize,
    len: JSize,
    buf: *mut JChar,
) {
    let _fx = ForeignJniEntry::enter();
    if str_obj == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        let utf16: Vec<u16> = s.encode_utf16().collect();
        // JNI contract: a region outside the string raises (String)IndexOutOf-
        // BoundsException — surfaced here as AIOOBE via the shared checker
        // (overflow-safe; no partial copy on a bad range).
        if !region_bounds_ok(start, len, utf16.len()) {
            return None;
        }
        let start = start as usize;
        let len = len as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(utf16[start..start + len].as_ptr(), buf, len);
        }
        Some(())
    });
}

// ---- Index 221: GetStringUTFRegion ----
extern "C" fn jni_get_string_utf_region(
    _env: JNIEnv,
    str_obj: JString,
    start: JSize,
    len: JSize,
    buf: *mut c_char,
) {
    let _fx = ForeignJniEntry::enter();
    if str_obj == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        // In JNI, start/len refer to UTF-16 code units.
        let utf16: Vec<u16> = s.encode_utf16().collect();
        // JNI contract: a region outside the string raises (String)IndexOutOf-
        // BoundsException — surfaced here as AIOOBE via the shared checker.
        if !region_bounds_ok(start, len, utf16.len()) {
            return None;
        }
        let start = start as usize;
        let len = len as usize;
        let region = String::from_utf16_lossy(&utf16[start..start + len]);
        // JNI writes *modified* UTF-8 here, exactly as `GetStringUTFChars`
        // does — same encoder, so the two entry points and
        // `GetStringUTFLength` (which measures with `modified_utf8_len`) all
        // agree. This used to take the region's *standard* UTF-8 bytes
        // directly, which disagreed with both siblings in two ways:
        //
        //   * U+0000 was written as a raw `0x00` instead of `0xC0 0x80`, so
        //     every C consumer saw the region truncated at the first interior
        //     NUL — `"a\0b"` read back as `"a"`.
        //   * A supplementary character was written as its 4-byte standard
        //     UTF-8 form instead of the 6-byte surrogate pair (CESU-8) the
        //     spec mandates. The canonical caller sizes its buffer with
        //     `GetStringUTFLength` (which correctly says 6) and then reads
        //     that many bytes, so bytes 4..6 were whatever `malloc` left
        //     there — an uninitialized read, and a string a modified-UTF-8
        //     decoder cannot parse.
        //
        // Residual (unchanged, and shared with `GetStringUTFChars`): a region
        // that splits a surrogate pair yields U+FFFD for the orphaned half,
        // because the round-trip goes through a Rust `str`. Same byte count,
        // so no buffer hazard.
        let bytes = to_modified_utf8(&region);
        unsafe {
            // Per the JNI spec, GetStringUTFRegion does NOT null-terminate the
            // destination buffer (unlike GetStringUTFChars). Writing a trailing
            // NUL would be a 1-byte overflow past a caller buffer sized exactly
            // to the region length, so copy only the region bytes. HotSpot
            // likewise writes no terminator here.
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
        }
        Some(())
    });
}

/// Metadata for the detached copy buffer `GetPrimitiveArrayCritical` hands out
/// (every array, not only G1-humongous ones). See `JNI_CRITICAL_COPIES`.
///
/// gc-common w2-c (2026-09-23), `common-g-pinning-semantics-differ-per-backend`:
/// the array is found again at Release through `array_gref`, a REMAPPABLE JNI
/// global ref, exactly as `ArrayElemBuffer` does for `Get<Type>ArrayElements`.
/// It used to keep the raw Get-time handle and re-resolve that, which was sound
/// only while the array could not move: G1 and ZGC pinned its region/page for
/// the whole section (`VmHeap::pin_critical_region`), Generational pinned
/// nothing -- so a Generational young copy inside the window (a foreign-attached
/// thread is GC-blocked between its calls; an up-call inside the section can
/// collect) made the copy-back write into from-space or a recycled object. With
/// the global ref the copy-back follows the array on every backend, the region
/// pin has nothing left to protect, and the three collectors honour one
/// contract: a detached copy, kept alive by a per-VM global ref, written back to
/// wherever the array is at Release. The keep-alive pin in the process-global
/// `cratonvm_gc::pinned` table went with it: the global ref is that root.
struct CriticalCopy {
    /// Remappable handle to the source array (a JNI global ref, bit 0 set).
    /// Resolved at Release to the array's current address; deleted on the
    /// final (freeing) release.
    array_gref: JObject,
    /// Element type, so release re-packs each element correctly.
    element_type: ArrayElementType,
    /// Number of elements (== array length at Get time).
    len: usize,
    /// Bytes per element (the contiguous buffer is `len * stride` bytes).
    stride: usize,
}

/// Size of the copy buffer `GetPrimitiveArrayCritical` hands out for `len`
/// elements of `stride` bytes: never 0, so every handout is a real,
/// uniquely-addressed allocation (gc-common w8-c; see the Get site).
fn critical_buffer_bytes(len: usize, stride: usize) -> usize {
    (len * stride).max(1)
}

/// Bytes per stored element for a primitive array element type.
fn critical_stride(et: ArrayElementType) -> usize {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        // Reference arrays are not valid for the primitive-critical API.
        ArrayElementType::Reference => 0,
    }
}

/// Write the host-endian bytes of array element `i` (read via the
/// region-safe accessor) into `dst[i*stride .. (i+1)*stride]`.
fn critical_encode_element(v: Value, et: ArrayElementType, dst: &mut [u8]) {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => {
            let x = v.as_int().unwrap_or(0) as i8;
            dst[0] = x as u8;
        }
        ArrayElementType::Short => {
            let x = v.as_int().unwrap_or(0) as i16;
            dst[..2].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Char => {
            let x = v.as_int().unwrap_or(0) as u16;
            dst[..2].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Int => {
            let x = v.as_int().unwrap_or(0);
            dst[..4].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Float => {
            let x = match v {
                Value::Float(f) => f,
                _ => 0.0,
            };
            dst[..4].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Long => {
            let x = match v {
                Value::Long(l) => l,
                _ => 0,
            };
            dst[..8].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Double => {
            let x = match v {
                Value::Double(d) => d,
                _ => 0.0,
            };
            dst[..8].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Reference => {}
    }
}

/// Decode element `i` from `src[i*stride .. (i+1)*stride]` (host-endian)
/// back into a `Value` for store via the region-safe accessor.
fn critical_decode_element(et: ArrayElementType, src: &[u8]) -> Value {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => Value::Int(src[0] as i8 as i32),
        ArrayElementType::Short => {
            let x = i16::from_ne_bytes([src[0], src[1]]);
            Value::Int(x as i32)
        }
        ArrayElementType::Char => {
            let x = u16::from_ne_bytes([src[0], src[1]]);
            Value::Int(x as i32)
        }
        ArrayElementType::Int => Value::Int(i32::from_ne_bytes([src[0], src[1], src[2], src[3]])),
        ArrayElementType::Float => {
            Value::Float(f32::from_ne_bytes([src[0], src[1], src[2], src[3]]))
        }
        ArrayElementType::Long => Value::Long(i64::from_ne_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ])),
        ArrayElementType::Double => Value::Double(f64::from_ne_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ])),
        ArrayElementType::Reference => Value::Object(None),
    }
}

// ---- Index 222: GetPrimitiveArrayCritical ----
// Returns a pointer to a native-endian COPY of the array data (is_copy=JNI_TRUE);
// see the `Get<Type>ArrayElements` module comment for why we never hand out a
// direct heap pointer under this VM's moving collectors.
extern "C" fn jni_get_primitive_array_critical(
    _env: JNIEnv,
    array: JArray,
    is_copy: *mut JBoolean,
) -> *mut std::ffi::c_void {
    let _fx = ForeignJniEntry::enter();
    if array == 0 {
        return std::ptr::null_mut();
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        // FORCE-COPY (vm-jni-roots #2): never hand out a raw pointer into the
        // live array body. The VM's moving collectors would relocate the array
        // out from under the native critical pointer (UAF). Materialise a
        // detached, native-endian copy via the region-safe per-element accessor,
        // register it for copy-back, and report `is_copy = JNI_TRUE`. The JNI
        // spec permits returning a copy from GetPrimitiveArrayCritical; it is the
        // only collector-agnostic correct choice for this heap. Works uniformly
        // for ordinary AND G1-humongous arrays.
        let element_type = shared.mem.heap.array_element_type(oref)?;
        let stride = critical_stride(element_type);
        if stride == 0 {
            return None; // reference array — not a primitive critical
        }
        let len = shared.mem.heap.array_length(oref);
        // gc-common w8-c: `.max(1)` (as `Get<Type>ArrayElements` already
        // does). A zero-length `vec!` does not allocate: its pointer is the
        // same dangling sentinel for every empty array, so two open criticals
        // on empty arrays (`in = Get(src); out = Get(dst)`, the compression
        // idiom) collided on one `JNI_CRITICAL_COPIES` key. The second insert
        // dropped the first entry, whose global ref -- the array's root --
        // was then never deleted: that array was immortal. Release rebuilds
        // the buffer with the same `critical_buffer_bytes`.
        //
        // gc-common w10-c: allocated FALLIBLY, and as an exact-length boxed
        // slice (Release rebuilds `(byte_len, byte_len)`). `vec![0u8; n]`
        // aborted the process when the C heap could not hold the copy; JNI's
        // answer is NULL with `OutOfMemoryError` pending.
        let byte_len = critical_buffer_bytes(len, stride);
        let mut buf: Vec<u8> = Vec::new();
        if buf.try_reserve_exact(byte_len).is_err() {
            raise_jni_oom(shared, "a GetPrimitiveArrayCritical copy");
            return None;
        }
        buf.resize(byte_len, 0);
        let mut buf: Box<[u8]> = buf.into_boxed_slice();
        for i in 0..len {
            let v = match shared.mem.heap.get_array_element(oref, i) {
                Ok(v) => v,
                Err(_) => break,
            };
            let off = i * stride;
            critical_encode_element(v, element_type, &mut buf[off..off + stride]);
        }
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf); // OWNERSHIP: transferred to native caller, reclaimed by jni_release_primitive_array_critical

        // Keep-alive AND find-again in one handle: a remappable global ref,
        // rewritten by every relocating collection, so the copy-back at Release
        // lands in the live array on every backend (see `CriticalCopy`). No
        // region/page pin: native code holds a copy, so the array is free to
        // move (gc-common w2-c).
        let array_gref = shared.natives.jni_global_refs.lock().add(oref);
        JNI_CRITICAL_COPIES.with(|c| {
            c.borrow_mut().insert(
                ptr as usize,
                CriticalCopy {
                    array_gref,
                    element_type,
                    len,
                    stride,
                },
            );
        });
        if !is_copy.is_null() {
            unsafe {
                *is_copy = JNI_TRUE;
            } // Always a copy under this VM's moving GC
        }
        Some(ptr as *mut std::ffi::c_void)
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// ---- Index 223: ReleasePrimitiveArrayCritical ----
extern "C" fn jni_release_primitive_array_critical(
    _env: JNIEnv,
    _array: JArray,
    carray: *mut std::ffi::c_void,
    mode: JInt,
) {
    let _fx = ForeignJniEntry::enter();
    if carray.is_null() {
        return;
    }
    // Every GetPrimitiveArrayCritical handout is a registered copy buffer. If
    // `carray` is not one we handed out (foreign pointer / already released),
    // there is nothing to copy back or free.
    let copy = JNI_CRITICAL_COPIES.with(|c| c.borrow_mut().remove(&(carray as usize)));
    let Some(copy) = copy else {
        return;
    };
    // Copy buffer. mode 0 = copy back + free, JNI_COMMIT (1) =
    // copy back, don't free, JNI_ABORT (2) = free without copy back.
    if mode != 2 {
        with_shared_vm(|shared| {
            // The array's CURRENT address (w2-c): the global ref followed any
            // relocation since Get. Bounded by the live length too, as
            // `Release<Type>ArrayElements` is.
            let oref = shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(copy.array_gref)?;
            let copy_len = copy.len.min(shared.mem.heap.array_length(oref));
            for i in 0..copy_len {
                let off = i * copy.stride;
                // SAFETY: the buffer is `copy.len * copy.stride` bytes and we
                // only read within it.
                let src = unsafe {
                    std::slice::from_raw_parts((carray as *const u8).add(off), copy.stride)
                };
                let v = critical_decode_element(copy.element_type, src);
                // Mint chokepoint for a `long[]` critical write-back.
                mint_if_smuggled_long_value(&shared.mem.heap, &v);
                let _ = shared.mem.heap.set_array_element(oref, i, v);
            }
            Some(())
        });
    }
    if mode != 1 {
        // Final release only (not JNI_COMMIT, whose section continues): drop
        // the global ref, i.e. the array's keep-alive root.
        with_shared_vm(|shared| {
            shared
                .natives
                .jni_global_refs
                .lock()
                .remove(copy.array_gref);
            Some(())
        });
        // Reconstruct the `Vec<u8>` with its original layout and drop it.
        let byte_len = critical_buffer_bytes(copy.len, copy.stride);
        // SAFETY: `carray` was produced by `Vec::<u8>::as_mut_ptr` +
        // `mem::forget` in `jni_get_primitive_array_critical` from
        // `vec![0u8; critical_buffer_bytes(len, stride)]`, so capacity ==
        // length == `byte_len`; we reconstruct the exact layout.
        unsafe {
            drop(Vec::from_raw_parts(carray as *mut u8, byte_len, byte_len));
        }
    } else {
        // JNI_COMMIT: we kept the buffer alive but already removed it from the
        // map; re-insert so a later release can still find it (its global ref
        // stays alive for the continuing section).
        JNI_CRITICAL_COPIES.with(|c| {
            c.borrow_mut().insert(carray as usize, copy);
        });
    }
}

// ---- Index 224: GetStringCritical ----
// HotSpot pins the string for the critical section; this VM never hands out a
// pointer into the heap (every collector here may move the string), so this is
// GetStringChars: a detached UTF-16 COPY, `is_copy = JNI_TRUE`, which the JNI
// spec permits. Nothing is pinned and no collection is held off.
// (gc-common w8-c: the comment said "our heap doesn't move objects between
// GC", which has not been true of any backend since the moving collectors.)
extern "C" fn jni_get_string_critical(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    jni_get_string_chars(_env, str_obj, is_copy)
}

// ---- Index 225: ReleaseStringCritical ----
extern "C" fn jni_release_string_critical(_env: JNIEnv, str_obj: JString, chars: *const JChar) {
    jni_release_string_chars(_env, str_obj, chars);
}

// ---- Index 226: NewWeakGlobalRef ----
// gc-common w8-c (`common-w7c-jni-weak-global-refs-are-strong`): a real weak
// global. It is kept in `JniGlobalRefs`' weak set, which is not a GC root; every
// collection remaps it or clears it (`sweep_weak_global_refs` and the concurrent
// remark / reclamation sweeps), and resolving it is a keep-alive. It used to be
// a strong global ref, so every object a native cached weakly (and the loader
// of every class it cached weakly) was immortal and never read as collected.
extern "C" fn jni_new_weak_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0;
    }
    // Same `jclass`-is-a-ClassId round trip as `NewGlobalRef` — and this is the
    // overload JNA's `LOAD_CREF` actually calls, so it is the one that decided
    // whether `com.sun.jna.Native` could initialise at all.
    if jclass_id_handle(obj) {
        return obj;
    }
    with_shared_vm(|shared| {
        // `jobject_to_obj` of a cleared weak `obj` is NULL, and so is the
        // result (HotSpot: a weak global to a collected object gives NULL).
        let oref = jobject_to_obj(obj)?;
        let mut refs = shared.natives.jni_global_refs.lock();
        Some(refs.add_weak(oref))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 227: DeleteWeakGlobalRef ----
extern "C" fn jni_delete_weak_global_ref(_env: JNIEnv, wref: JObject) {
    let _fx = ForeignJniEntry::enter();
    if wref == 0 {
        return;
    }
    // A class handle is permanent; dropping it is a no-op (see DeleteGlobalRef).
    if is_jclass_handle(wref) {
        return;
    }
    with_shared_vm(|shared| {
        let mut refs = shared.natives.jni_global_refs.lock();
        refs.remove(wref);
        Some(())
    });
}

// ---- Index 19: GetObjectRefType ----
// Returns the type of the reference: JNIInvalidRefType=0, JNILocalRefType=1,
// JNIGlobalRefType=2, JNIWeakGlobalRefType=3.
extern "C" fn jni_get_object_ref_type(_env: JNIEnv, obj: JObject) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0; // JNIInvalidRefType
    }
    // A `jclass` handle carries `JCLASS_TAG`, not the global-ref bit-0 tag, and
    // its low bit is just the low bit of the ClassId — so without this it
    // answered global-or-local at random per class. `FindClass` hands back a
    // local ref on HotSpot.
    if is_jclass_handle(obj) {
        return 1; // JNILocalRefType
    }
    if obj & 1 == 1 {
        // gc-common w8-c: a weak global answers JNIWeakGlobalRefType (3), as
        // the spec says; anything else with the tag keeps the old answer, 2.
        let weak = with_shared_vm(|shared| {
            shared.natives.jni_global_refs.lock().kind(obj) == Some(JniGlobalKind::Weak)
        })
        .unwrap_or(false);
        if weak {
            3 // JNIWeakGlobalRefType
        } else {
            2 // JNIGlobalRefType
        }
    } else {
        1 // JNILocalRefType
    }
}

/// Stub for unimplemented JNI functions. Logs a warning and returns 0.
extern "C" fn jni_stub() -> usize {
    tracing::warn!("unimplemented JNI function called");
    0
}

/// Stub for the bare C-varargs (`...`) JNI call slots — `CallObjectMethod`,
/// `CallStatic<Type>Method`, `CallNonvirtual<Type>Method`, etc.
///
/// The `...`-taking forms cannot be dispatched in stable Rust: there is no
/// portable way to walk a platform `va_list` that the caller assembled inline
/// (the V/`*MethodV` form receives an explicit `va_list` and the A/`*MethodA`
/// form receives a `jvalue[]`, both of which *are* implemented and wired).
///
/// Rather than silently fabricating a `0`/null result (which a native would
/// mistake for a real return value — an empty string, a null object, a zero
/// count), this raises an `UnsatisfiedLinkError` so the unsupported call fails
/// loudly. Native callers should use the `*MethodV` / `*MethodA` variants.
extern "C" fn jni_varargs_unsupported() -> usize {
    let _fx = ForeignJniEntry::enter();
    jni_throw_unsatisfied_link(
        "bare C-varargs JNI call form (CallXxxMethod(...)) is not supported on this VM; \
         use the CallXxxMethodV (va_list) or CallXxxMethodA (jvalue[]) variant instead",
    );
    0
}

// ---------------------------------------------------------------------------
// Bare-varargs Call<Type>Method(...) trampolines
// ---------------------------------------------------------------------------
//
// The `...` forms are a third of the JNI call surface and cannot be WRITTEN in
// stable Rust — defining a C-variadic function is still unstable (rust-lang
// #44930). They can, however, be ENTERED in assembly: a variadic callee's only
// extra obligation over an ordinary one is to materialise the `va_list` its ABI
// describes, and once it has, the `...MethodV` implementation already in this
// file does the rest. So every `...` slot gets a small trampoline that builds
// the platform `va_list` and hands off to its `V` sibling.
//
// `call` then `leave`/`ret`, not a tail `jmp`: the `va_list` and its register
// save area live in THIS frame and must outlive the callee. The return value
// needs no handling — RAX and XMM0 pass straight through — which is also why
// one trampoline shape serves `jint`, `jlong`, `jobject`, `jdouble` and `void`
// alike.
//
// Two named-argument counts cover every slot:
//   3 named — `(env, obj|clazz, methodID, ...)`: NewObject, Call<T>Method,
//             CallStatic<T>Method
//   4 named — `(env, obj, clazz, methodID, ...)`: CallNonvirtual<T>Method
//
// Anything that is not x86-64 keeps `jni_varargs_unsupported`, which raises
// UnsatisfiedLinkError rather than fabricating a 0/null.

/// System V x86-64: spill the six GP and eight SSE argument registers into a
/// 176-byte register-save area, then hand the callee a `__va_list_tag` whose
/// `gp_offset` already skips the named arguments.
///
/// Frame after `push rbp; sub rsp,224` — RSP stays 16-aligned, which `movaps`
/// requires:
///   `[rsp    .. rsp+23 ]` the `__va_list_tag`
///   `[rsp+32 .. rsp+207]` the register-save area (6x8 GP, then 8x16 SSE)
///
/// `AL` carries the caller's count of SSE argument registers used, so the
/// `movaps` block is skipped entirely for the common all-integer call.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
macro_rules! jni_varargs_trampoline {
    (@n 3, $sym:ident, $target:path) => {
        jni_varargs_trampoline!(@emit $sym, $target, "24", "lea rcx, [rsp]", "");
    };
    (@n 4, $sym:ident, $target:path) => {
        jni_varargs_trampoline!(@emit $sym, $target, "32", "mov rcx, [rsp+56]", "lea r8, [rsp]");
    };
    (@emit $sym:ident, $target:path, $gp_off:literal, $tail1:literal, $tail2:literal) => {
        core::arch::global_asm!(
            ".p2align 4",
            concat!(".globl ", stringify!($sym)),
            concat!(stringify!($sym), ":"),
            "push rbp",
            "mov rbp, rsp",
            "sub rsp, 224",
            "mov [rsp+32], rdi",
            "mov [rsp+40], rsi",
            "mov [rsp+48], rdx",
            "mov [rsp+56], rcx",
            "mov [rsp+64], r8",
            "mov [rsp+72], r9",
            "test al, al",
            "je 1f",
            "movaps [rsp+80], xmm0",
            "movaps [rsp+96], xmm1",
            "movaps [rsp+112], xmm2",
            "movaps [rsp+128], xmm3",
            "movaps [rsp+144], xmm4",
            "movaps [rsp+160], xmm5",
            "movaps [rsp+176], xmm6",
            "movaps [rsp+192], xmm7",
            "1:",
            concat!("mov dword ptr [rsp], ", $gp_off), // gp_offset skips the named args
            "mov dword ptr [rsp+4], 48",               // fp_offset: start of the SSE area
            "lea rax, [rbp+16]",                       // overflow_arg_area = 1st stack arg
            "mov [rsp+8], rax",
            "lea rax, [rsp+32]",                       // reg_save_area
            "mov [rsp+16], rax",
            "mov rdi, [rsp+32]",                       // re-load the named args
            "mov rsi, [rsp+40]",
            "mov rdx, [rsp+48]",
            $tail1,
            $tail2,
            "call {target}",
            "leave",
            "ret",
            target = sym $target,
        );
    };
}

/// Windows x64: every argument, named or variadic, arrives in RCX/RDX/R8/R9
/// then on the stack, and the caller always reserved the 32-byte home area at
/// `[rsp+8]`. Spilling the four register arguments into their home slots makes
/// the whole argument list one contiguous run of 8-byte slots — which IS the
/// Windows `va_list`. A `double` vararg is passed in both its GP register and
/// its XMM register, so the GP spill carries the bits.
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
macro_rules! jni_varargs_trampoline {
    (@n 3, $sym:ident, $target:path) => {
        // 3 named: RCX/RDX/R8 already hold them; the va_list is the 4th arg.
        jni_varargs_trampoline!(@emit $sym, $target, "lea rax, [rsp+32]",
                                "sub rsp, 32", "mov r9, rax");
    };
    (@n 4, $sym:ident, $target:path) => {
        // 4 named: all four registers are named, so the va_list is the 5th arg
        // and goes on the stack just above the 32-byte shadow space.
        jni_varargs_trampoline!(@emit $sym, $target, "lea rax, [rsp+40]",
                                "sub rsp, 48", "mov [rsp+32], rax");
    };
    (@emit $sym:ident, $target:path, $lea:literal, $tail1:literal, $tail2:literal) => {
        core::arch::global_asm!(
            ".p2align 4",
            concat!(".globl ", stringify!($sym)),
            concat!(stringify!($sym), ":"),
            "mov [rsp+8], rcx",  // home slots, in the caller's shadow space
            "mov [rsp+16], rdx",
            "mov [rsp+24], r8",
            "mov [rsp+32], r9",
            $lea,                // rax = &first variadic argument
            "push rbp",
            "mov rbp, rsp",
            $tail1,              // shadow space (+ the 5th arg slot when needed)
            $tail2,
            "call {target}",
            "mov rsp, rbp",
            "pop rbp",
            "ret",
            target = sym $target,
        );
    };
}

#[cfg(target_arch = "x86_64")]
macro_rules! jni_varargs_trampolines {
    ($( $sym:ident => ($target:path, $named:tt) ),* $(,)?) => {
        $( jni_varargs_trampoline!(@n $named, $sym, $target); )*
        extern "C" {
            $(
                /// Assembly trampoline; only ever used for its ADDRESS.
                fn $sym();
            )*
        }
    };
}

#[cfg(target_arch = "x86_64")]
jni_varargs_trampolines! {
    jni_va_new_object => (jni_new_object_v, 3),

    jni_va_call_object_method  => (jni_call_object_method_v, 3),
    jni_va_call_boolean_method => (jni_call_boolean_method_v, 3),
    jni_va_call_byte_method    => (jni_call_byte_method_v, 3),
    jni_va_call_char_method    => (jni_call_char_method_v, 3),
    jni_va_call_short_method   => (jni_call_short_method_v, 3),
    jni_va_call_int_method     => (jni_call_int_method_v, 3),
    jni_va_call_long_method    => (jni_call_long_method_v, 3),
    jni_va_call_float_method   => (jni_call_float_method_v, 3),
    jni_va_call_double_method  => (jni_call_double_method_v, 3),
    jni_va_call_void_method    => (jni_call_void_method_v, 3),

    jni_va_call_nonvirtual_object_method  => (jni_call_nonvirtual_object_method_v, 4),
    jni_va_call_nonvirtual_boolean_method => (jni_call_nonvirtual_boolean_method_v, 4),
    jni_va_call_nonvirtual_byte_method    => (jni_call_nonvirtual_byte_method_v, 4),
    jni_va_call_nonvirtual_char_method    => (jni_call_nonvirtual_char_method_v, 4),
    jni_va_call_nonvirtual_short_method   => (jni_call_nonvirtual_short_method_v, 4),
    jni_va_call_nonvirtual_int_method     => (jni_call_nonvirtual_int_method_v, 4),
    jni_va_call_nonvirtual_long_method    => (jni_call_nonvirtual_long_method_v, 4),
    jni_va_call_nonvirtual_float_method   => (jni_call_nonvirtual_float_method_v, 4),
    jni_va_call_nonvirtual_double_method  => (jni_call_nonvirtual_double_method_v, 4),
    jni_va_call_nonvirtual_void_method    => (jni_call_nonvirtual_void_method_v, 4),

    jni_va_call_static_object_method  => (jni_call_static_object_method_v, 3),
    jni_va_call_static_boolean_method => (jni_call_static_boolean_method_v, 3),
    jni_va_call_static_byte_method    => (jni_call_static_byte_method_v, 3),
    jni_va_call_static_char_method    => (jni_call_static_char_method_v, 3),
    jni_va_call_static_short_method   => (jni_call_static_short_method_v, 3),
    jni_va_call_static_int_method     => (jni_call_static_int_method_v, 3),
    jni_va_call_static_long_method    => (jni_call_static_long_method_v, 3),
    jni_va_call_static_float_method   => (jni_call_static_float_method_v, 3),
    jni_va_call_static_double_method  => (jni_call_static_double_method_v, 3),
    jni_va_call_static_void_method    => (jni_call_static_void_method_v, 3),
}

/// The 31 bare-varargs slots, paired with what serves each: the trampoline on
/// x86-64, `jni_varargs_unsupported` everywhere else.
#[cfg(target_arch = "x86_64")]
fn jni_varargs_slots() -> [(usize, usize); 31] {
    macro_rules! slot {
        ($idx:expr, $sym:ident) => {
            ($idx, $sym as *const () as usize)
        };
    }
    [
        slot!(28, jni_va_new_object),
        slot!(34, jni_va_call_object_method),
        slot!(37, jni_va_call_boolean_method),
        slot!(40, jni_va_call_byte_method),
        slot!(43, jni_va_call_char_method),
        slot!(46, jni_va_call_short_method),
        slot!(49, jni_va_call_int_method),
        slot!(52, jni_va_call_long_method),
        slot!(55, jni_va_call_float_method),
        slot!(58, jni_va_call_double_method),
        slot!(61, jni_va_call_void_method),
        slot!(64, jni_va_call_nonvirtual_object_method),
        slot!(67, jni_va_call_nonvirtual_boolean_method),
        slot!(70, jni_va_call_nonvirtual_byte_method),
        slot!(73, jni_va_call_nonvirtual_char_method),
        slot!(76, jni_va_call_nonvirtual_short_method),
        slot!(79, jni_va_call_nonvirtual_int_method),
        slot!(82, jni_va_call_nonvirtual_long_method),
        slot!(85, jni_va_call_nonvirtual_float_method),
        slot!(88, jni_va_call_nonvirtual_double_method),
        slot!(91, jni_va_call_nonvirtual_void_method),
        slot!(114, jni_va_call_static_object_method),
        slot!(117, jni_va_call_static_boolean_method),
        slot!(120, jni_va_call_static_byte_method),
        slot!(123, jni_va_call_static_char_method),
        slot!(126, jni_va_call_static_short_method),
        slot!(129, jni_va_call_static_int_method),
        slot!(132, jni_va_call_static_long_method),
        slot!(135, jni_va_call_static_float_method),
        slot!(138, jni_va_call_static_double_method),
        slot!(141, jni_va_call_static_void_method),
    ]
}

/// See the x86-64 arm: same slot indices, all refusing loudly.
#[cfg(not(target_arch = "x86_64"))]
fn jni_varargs_slots() -> [(usize, usize); 31] {
    let refuse = jni_varargs_unsupported as *const () as usize;
    let mut out = [(0usize, refuse); 31];
    let idx = [
        28, 34, 37, 40, 43, 46, 49, 52, 55, 58, 61, 64, 67, 70, 73, 76, 79, 82, 85, 88, 91, 114,
        117, 120, 123, 126, 129, 132, 135, 138, 141,
    ];
    for (o, i) in out.iter_mut().zip(idx) {
        o.0 = i;
    }
    out
}

// ---------------------------------------------------------------------------
// Internal helpers for field access
// ---------------------------------------------------------------------------

fn get_int_field_raw(obj: JObject, field_id: JFieldID) -> JInt {
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Int(i) => Some(i),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

fn set_int_field_raw(obj: JObject, field_id: JFieldID, val: i32) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .mem
            .heap
            .set_field(oref, field_index, Value::Int(val));
        Some(())
    });
}

fn get_static_int_raw(clazz: JClass, field_id: JFieldID) -> JInt {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Int(i)) => Some(*i),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

fn set_static_int_raw(field_id: JFieldID, val: JInt) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Int(val);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// JNI Native Method Registry (RegisterNatives)
// ---------------------------------------------------------------------------

/// C-compatible struct matching `JNINativeMethod` in jni.h.
/// Used by `RegisterNatives` to pass (name, signature, function pointer) triples.
#[repr(C)]
struct JNINativeMethod {
    name: *const c_char,
    signature: *const c_char,
    fn_ptr: *mut (),
}

// Safety: the pointers in JNINativeMethod are only read while holding the
// caller's guarantee that they're valid (they're passed in from C). The struct
// itself is never stored.

/// The table type behind `SharedVm::natives::jni_native_methods`.
///
/// Key = FNV-1a hash of `"class_name.method_nameDescriptor"`, value = the raw
/// `fn` address inside the host library, called through `dispatch_jni_native`.
///
/// This was the process global `static JNI_NATIVE_METHODS` here until
/// 2026-08-06 — `JDK-ONLY-WAVE2` §6 (retired record:
/// additional-wave2-markers-not-in-the-original-inventory.md).
/// Contract §2 forbids process globals for this feature's state, and the
/// concrete hazard was that two VMs in one process saw each other's
/// `RegisterNatives`: a library loaded by VM A bound its pointers for VM B too.
///
/// JDK-only mode: this remains a **second, parallel native registry** and is
/// deliberately left as one. Everything in it is a real function pointer inside
/// a real `.so`/`.dll` that a real JNI library published — exactly the "native
/// code may cross VM boundaries" case §11 sanctions. There is no `NativeKind`
/// to attach because there is no CratonVM-authored implementation to classify:
/// a `NativeKind::SyntheticStub` cannot get in here, so this table cannot be
/// the §1.3 bypass the single-resolver rule targets. Its dispatch sites in
/// `vm/src/vm/vm_exec.rs` consult `resolve_dispatch` before falling through to
/// [`find_jni_native`].
///
/// The census gap the move left, CLOSED 2026-08-10 without minting a fake id.
/// `record_invocation` keys on a `NativeMethodId` and only
/// `NativeMethodRegistry` issues one, so there is still no honest way to give a
/// `dlsym` result an id — and none is invented. Instead both dispatch sites in
/// `vm/src/vm/vm_exec.rs` increment `NativeRealm::jni_bridge_invocations`, and
/// `--jdk-only-report` adds that to `bridge_invocations`, which is where a real
/// function in a real library belongs. Until then that key under-counted
/// genuine JNI bridges by exactly the number of dispatches through this table.
/// `synthetic_stub_invocations` was and stays exact: a stub can never be here.
pub type JniNativeMethodTable = parking_lot::RwLock<HashMap<u64, usize>>;

/// Compute a hash key for a (class, method, descriptor) triple.
/// Identical algorithm to `NativeMethodRegistry::native_method_hash`.
fn jni_native_key(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in class_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in method_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in descriptor.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Store a JNI function pointer in a specific table.
///
/// The `_in` pair is where the logic lives so a test can hand in a table
/// without standing up a whole `NativeRealm` — and, more usefully, so the
/// per-VM isolation this move exists for is directly testable with two of them.
pub fn register_jni_native_in(
    table: &JniNativeMethodTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
) {
    let key = jni_native_key(class_name, method_name, descriptor);
    let mut table = table.write();
    if let Some(&existing) = table.get(&key) {
        if existing != fn_ptr {
            tracing::warn!(
                "JNI native method hash collision or re-registration: {}.{}{} (replacing 0x{:x} with 0x{:x})",
                class_name, method_name, descriptor, existing, fn_ptr
            );
        }
    }
    table.insert(key, fn_ptr);
}

/// Look up a JNI function pointer in a specific table.
pub fn find_jni_native_in(
    table: &JniNativeMethodTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let key = jni_native_key(class_name, method_name, descriptor);
    table.read().get(&key).copied()
}

/// Store a JNI function pointer registered via `RegisterNatives` or symbol
/// lookup, in **this VM's** table.
pub fn register_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
) {
    register_jni_native_in(
        &natives.jni_native_methods,
        class_name,
        method_name,
        descriptor,
        fn_ptr,
    );
    // gce e1/j: after the write, so a memo validated against the new
    // generation can only have been filled from the new table.
    natives
        .jni_native_generation
        .fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Look up a JNI function pointer for the given method in **this VM's** table.
/// Returns `None` if no pointer was registered.
///
/// gce e1/j (`gcd-d5f-jni-default-dispatch-costs-500ns`, item 1): a hit is
/// memoised per thread ([`JniFnMemo`], one entry) and served while the table's
/// registration generation ([`NativeRealm::jni_native_generation`]) is
/// unchanged -- three string compares and one atomic load instead of the
/// FNV hash of the triple, the table's read lock and the map probe, on every
/// dispatch of the same native from a loop. A miss is never memoised.
///
/// [`NativeRealm::jni_native_generation`]: crate::vm::realms::NativeRealm::jni_native_generation
pub fn find_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let realm = natives as *const crate::vm::realms::NativeRealm as usize;
    // Read BEFORE the table: a registration racing with the lookup below then
    // leaves the memo on the old generation, which the next read refuses.
    let generation = natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let memo_hit = JNI_NATIVE_CALL
        .try_with(|t| {
            let memo = t.fn_memo.try_borrow().ok()?;
            memo.as_ref()
                .filter(|m| m.matches(realm, generation, class_name, method_name, descriptor))
                .map(|m| m.fn_ptr)
        })
        .ok()
        .flatten();
    if memo_hit.is_some() {
        return memo_hit;
    }
    let found = find_jni_native_in(
        &natives.jni_native_methods,
        class_name,
        method_name,
        descriptor,
    )?;
    let _ = JNI_NATIVE_CALL.try_with(|t| {
        if let Ok(mut memo) = t.fn_memo.try_borrow_mut() {
            let m = memo.get_or_insert_with(JniFnMemo::default);
            m.fill(realm, generation, class_name, method_name, descriptor, found);
        }
    });
    Some(found)
}

/// gce e1/j: this thread's last [`find_jni_native`] hit. The strings are
/// compared by value (a `Class.method` name pair and a descriptor), the realm
/// by address and the generation, which is unique per VM, by value.
#[derive(Default)]
struct JniFnMemo {
    realm: usize,
    generation: u64,
    class_name: String,
    method_name: String,
    descriptor: String,
    fn_ptr: usize,
}

impl JniFnMemo {
    fn matches(
        &self,
        realm: usize,
        generation: u64,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        self.fn_ptr != 0
            && self.realm == realm
            && self.generation == generation
            && self.method_name == method_name
            && self.descriptor == descriptor
            && self.class_name == class_name
    }

    /// Refill in place (the three `String` buffers are reused).
    fn fill(
        &mut self,
        realm: usize,
        generation: u64,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        fn_ptr: usize,
    ) {
        self.realm = realm;
        self.generation = generation;
        self.class_name.clear();
        self.class_name.push_str(class_name);
        self.method_name.clear();
        self.method_name.push_str(method_name);
        self.descriptor.clear();
        self.descriptor.push_str(descriptor);
        self.fn_ptr = fn_ptr;
    }
}

// ---------------------------------------------------------------------------
// JNI name mangling (JNI spec §11.3)
// ---------------------------------------------------------------------------

/// Encode a single component (class name or method name) for JNI symbol lookup.
/// JNI encoding rules:
///   `/` → `_`   (package separator)
///   `_` → `_1`  (literal underscore)
///   `;` → `_2`  (descriptor separator)
///   `[` → `_3`  (array prefix)
///   Unicode `\uXXXX` → `_0XXXX`
fn jni_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        match ch {
            '/' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            // JNI spec 11.3: only alphanumerics survive as themselves. EVERY
            // other character takes the `_0XXXX` escape — it is not a
            // non-ASCII escape.
            //
            // This arm used to be `c if c.is_ascii() => out.push(c)`, which
            // passed `$` through verbatim. `$` is the separator in every
            // nested class's binary name, so for
            // `JdkOnlyPlatformProbe$JniProbe.add` the VM looked up
            // `Java_JdkOnlyPlatformProbe$JniProbe_add` while the compiler had
            // emitted `Java_JdkOnlyPlatformProbe_00024JniProbe_add` — verified
            // against `nm -D` on a library HotSpot binds from the same file.
            // dlsym never matched, and the method raised UnsatisfiedLinkError
            // with the library loaded and the symbol present. NO native on a
            // nested or inner class could bind by name, which is most of them:
            // the conventional shape is a package-private nested holder.
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => {
                // Unicode escape: _0XXXX
                out.push_str(&format!("_0{:04x}", c as u32));
            }
        }
    }
    out
}

/// Build the JNI short name: `Java_<class>_<method>`.
///
/// Example: `("java/lang/System", "arraycopy")` → `"Java_java_lang_System_arraycopy"`.
pub fn jni_short_name(class_name: &str, method_name: &str) -> String {
    format!(
        "Java_{}_{}",
        jni_encode(class_name),
        jni_encode(method_name)
    )
}

/// Build the JNI long name: `Java_<class>_<method>__<encoded_params>`.
///
/// The parameter portion is the substring between `(` and `)` in the descriptor,
/// encoded with the same rules plus `;` → `_2` and `[` → `_3`.
///
/// Example: `("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V")`
///   → `"Java_java_lang_System_arraycopy__Ljava_lang_Object_2ILjava_lang_Object_2II"`.
pub fn jni_long_name(class_name: &str, method_name: &str, descriptor: &str) -> String {
    let params = descriptor
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    format!(
        "Java_{}_{}__{}",
        jni_encode(class_name),
        jni_encode(method_name),
        jni_encode(params)
    )
}

/// Try to resolve a native method by JNI naming convention in loaded libraries.
///
/// Tries the short name first, then the long name (JNI spec §11.3 resolution order).
/// If found, the function pointer is cached in `JNI_NATIVE_METHODS` for future lookups.
pub fn resolve_jni_native_in_libraries(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let short = jni_short_name(class_name, method_name);
    let long = jni_long_name(class_name, method_name, descriptor);

    let libs = natives.native_libraries.lock();
    for symbol_name in [&short, &long] {
        let c_name = match std::ffi::CString::new(symbol_name.as_bytes()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for lib in libs.iter() {
            // Safety: we are looking up a symbol name that follows JNI conventions.
            if let Ok(sym) = unsafe { lib.get::<*const ()>(c_name.as_bytes_with_nul()) } {
                let fn_ptr = *sym as usize;
                if fn_ptr != 0 {
                    // Cache for future lookups
                    drop(libs);
                    register_jni_native(natives, class_name, method_name, descriptor, fn_ptr);
                    tracing::info!(
                        symbol = %symbol_name,
                        method = %format!("{}.{}{}", class_name, method_name, descriptor),
                        "Auto-resolved native method via JNI naming convention"
                    );
                    return Some(fn_ptr);
                }
            }
        }
    }
    None
}

/// The `jlong` bits for one Java `long` argument of a JNI call — with an
/// `Unsafe`-arena handle translated to the real address it names.
///
/// This is the second door of the problem
/// [`direct_buffer_native_address`](fn@direct_buffer_native_address) documents
/// for the first. `Unsafe.allocateMemory` on this VM returns a tagged handle,
/// not an OS pointer; `GetDirectBufferAddress` already translates one before
/// handing it to C, but a library that never calls that entry point and instead
/// takes the address as a plain `long` parameter got the raw handle. Netty's
/// `netty-tcnative` is written exactly that way — `SSL.bioWrite(long bio, long
/// address, int len)` — so the first BIO write of the first TLS handshake
/// reached `BUF_MEM_append` with `0x4000_0010_0000_0010` in RSI and took a
/// SIGSEGV inside BoringSSL, with no CratonVM frame at the fault to say why.
///
/// Only a value that is BOTH tagged (bit 62 — never set on a real pointer on
/// any target this VM builds for) AND inside a live arena block is rewritten,
/// so a genuine `long` payload is untouched unless it lands in the few-GiB
/// window of currently-live arena addresses, which no real datum does.
///
/// A tagged handle that no live block covers — a use-after-free, or an offset
/// past the end — is passed through unchanged rather than silently redirected
/// to some other block: the native then faults on the handle, which is the
/// same visible failure it has today, instead of quietly reading the wrong
/// object's bytes.
#[inline]
fn jni_long_arg_bits(raw: i64) -> u64 {
    if !cratonvm_native_builtins::unsafe_arena_addr_is_tagged(raw) {
        return raw as u64;
    }
    match cratonvm_native_builtins::unsafe_arena_real_ptr(raw) {
        Some((ptr, _remaining)) => ptr as usize as u64,
        None => {
            tracing::warn!(
                handle = format!("{raw:#x}"),
                "JNI long argument is a dead Unsafe-arena handle — passing it through untranslated"
            );
            raw as u64
        }
    }
}

/// Dispatch a JNI native function pointer call.
///
/// Converts `Value` args to 64-bit C values (correct for all integer/reference
/// types on x86-64). Float/double args are passed as their bit representation
/// in integer registers — this is ABI-correct on Windows x64, but not on
/// Linux x86-64 System V for the float/double parameter positions.
///
/// Java `long` arguments pass through [`jni_long_arg_bits`], which turns an
/// `Unsafe`-arena handle into the real address it names — see that function for
/// why the `GetDirectBufferAddress` translation alone was not enough.
///
/// # Safety
/// `fn_ptr` must be a valid JNI native function whose signature matches
/// `descriptor`, and the TLS JNI context must already be set.
pub unsafe fn dispatch_jni_native(
    fn_ptr: usize,
    env: JNIEnv,
    receiver: JObject,
    args: &[Value],
    descriptor: &str,
) -> Value {
    dispatch_jni_native_census(fn_ptr, env, receiver, args, descriptor, None)
}

/// [`dispatch_jni_native`] with the dispatching VM's JNI phase census
/// ([`JniPhaseCensus`], gce e1/j): `vm_exec`'s JNI door passes it, so the
/// phases inside the dispatch are timed without a thread-local lookup. `None`
/// or a census that is off times nothing.
///
/// # Safety
/// As [`dispatch_jni_native`].
pub(crate) unsafe fn dispatch_jni_native_census(
    fn_ptr: usize,
    env: JNIEnv,
    receiver: JObject,
    args: &[Value],
    descriptor: &str,
    census: Option<&JniPhaseCensus>,
) -> Value {
    let census = census.filter(|c| c.on);
    let mut clock = census.and_then(JniPhaseCensus::start);
    let lap = |phase: JniPhase, clock: &mut Option<std::time::Instant>| {
        if let Some(c) = census {
            c.lap(phase, clock);
        }
    };
    // gcd d10/j: the tags in place (`param_type_tags`), not a cached copy.
    let param_types = param_type_tags(descriptor);
    // gcd d3/k (`CRATONVM_JNI_NATIVE_TRANSITIONS`): read before the receiver
    // and the arguments are recorded, so a raw one among them keeps this call
    // out of native (`JniNativeCall`).
    let escapes_before = raw_local_escapes();

    // Build a type-tagged argument list. `env` and `receiver` are always the
    // first two integer-class arguments; the Java args follow, each tagged
    // integer (GP register class) or floating-point (SSE register class) so the
    // platform calling convention can route them to the correct registers.
    // gcd d10/j: on the stack up to 14 Java arguments (a heap `Vec` per call
    // before); the trampoline reads a slice either way.
    let mut jargs: smallvec::SmallVec<[JniArg; 16]> =
        smallvec::SmallVec::with_capacity(args.len() + 2);
    // `env` is a raw pointer (`*const *const usize`); flatten it to a 64-bit
    // integer-class word. `receiver` is already a `JObject` (u64 handle).
    jargs.push(JniArg::int(env as usize as u64));
    // The receiver is a local ref for the duration of the call, like the
    // object arguments below (gc-common w3-g; `new_local_handle`). A static
    // native's `jclass` is skipped by `record_local_handle`. With
    // `CRATONVM_JNI_INDIRECT_LOCALS` the native gets an indirect handle to it
    // (gc-common w6-g); a `jclass` comes back unchanged.
    let receiver = if indirect_locals_active() {
        record_local_handle_indirect(receiver)
    } else {
        record_local_handle(receiver);
        receiver
    };
    jargs.push(JniArg::int(receiver));
    for (v, tag) in args.iter().zip(param_types) {
        let a = match v {
            Value::Int(i) => JniArg::int(*i as i64 as u64),
            Value::Long(l) => JniArg::int(jni_long_arg_bits(*l)),
            // Float/double are SSE-class: their bit pattern must go in an XMM
            // register on SysV (and in the positionally-shared XMM slot on
            // Win64), not in a GP register.
            Value::Float(f) => JniArg::float(f.to_bits() as u64),
            Value::Double(d) => JniArg::float(d.to_bits()),
            Value::Object(Some(r)) => JniArg::int(new_local_handle(*r)),
            Value::Object(None) => JniArg::int(0u64),
            // Defensive: an operand of an unexpected variant. Fall back to the
            // declared descriptor tag to keep the register class correct.
            _ => {
                if tag == b'F' || tag == b'D' {
                    JniArg::float(0)
                } else {
                    JniArg::int(0)
                }
            }
        };
        jargs.push(a);
    }

    // Whether the return value comes back in XMM0 (F/D) rather than RAX.
    let ret_char = descriptor
        .rfind(')')
        .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
        .unwrap_or(b'V');
    let fp_return = ret_char == b'F' || ret_char == b'D';

    // gcd d3/k: the C call runs in native (GC-safe) when the flag is on and
    // every local it can hold is indirect; the guard leaves native BEFORE the
    // result below is decoded, so an indirect result resolves through the
    // remapped table. Inert (one latched load) with the flag off.
    // gcd d4/k2 (lane m's request 1): count this call in the VM's pause
    // ledger while the native may hold RAW locals -- decided here, after the
    // receiver and arguments were recorded, so a raw one among them counts.
    // Held to the end of the dispatch, i.e. past the in-native leave below.
    // One cached flag load unless the pinned young copy or option B is on.
    lap(JniPhase::ArgMarshal, &mut clock);
    let _raw_locals = RawLocalsDispatch::enter();
    lap(JniPhase::RawLocals, &mut clock);
    let in_native = JniNativeCall::enter(escapes_before);
    lap(JniPhase::EnterNative, &mut clock);
    let raw_result = call_jni_marshalled(fn_ptr, &jargs[..], fp_return);
    lap(JniPhase::NativeBody, &mut clock);
    drop(in_native);
    lap(JniPhase::LeaveNative, &mut clock);
    // Charged at every return below (a `Drop` guard: the match returns).
    struct DecodeClock<'a>(Option<&'a JniPhaseCensus>, Option<std::time::Instant>);
    impl Drop for DecodeClock<'_> {
        fn drop(&mut self) {
            if let Some(c) = self.0 {
                c.stop(JniPhase::ResultDecode, self.1);
            }
        }
    }
    let _decode_clock = DecodeClock(census, clock);

    // `ret_char` was already computed above to decide `fp_return`.
    match ret_char {
        b'V' => Value::Object(None),
        // `jboolean` is a byte, and Java's `boolean` is 0 or 1, so the return
        // has to be normalised — but by VALUE, the way HotSpot's native
        // wrapper does it (`movzbl` then `setne`), not by the low BIT. A
        // native that returns a flag word rather than a literal `JNI_TRUE`
        // — `return flags & MASK;` is ordinary C — hands back something like
        // `0x80`, which `& 1` reads as FALSE and HotSpot reads as true.
        b'Z' => Value::Int((raw_result as u8 != 0) as i32),
        b'B' => Value::Int(raw_result as i8 as i32),
        b'C' => Value::Int(raw_result as u16 as i32),
        b'S' => Value::Int(raw_result as i16 as i32),
        b'I' => Value::Int(raw_result as i32),
        b'J' => {
            // Long-smuggle mint chokepoint: a native that received a raw
            // jobject handle (obj_to_jobject = the object's address) may echo
            // it back as its jlong return — the classic long-as-jobject
            // smuggle. Register the exact value so the GC's value-stack
            // rewrite arm can distinguish this genuine handle from a
            // primitive long that merely collides with a heap address (which
            // must NOT be rewritten). Strict object-start probe: only real
            // handles register; ordinary numeric returns don't look like
            // aligned live object bases.
            let bits = raw_result;
            if bits != 0 && bits & 0x7 == 0 {
                with_shared_vm(|shared| {
                    if shared.mem.heap.is_object_address(bits as usize).is_some() {
                        crate::memory::smuggled_longs::record_minted_long(&shared.mem.heap, bits);
                    }
                    Some(())
                });
            }
            Value::Long(raw_result as i64)
        }
        b'F' => Value::Float(f32::from_bits(raw_result as u32)),
        b'D' => Value::Double(f64::from_bits(raw_result)),
        _ => match jobject_to_obj(raw_result) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    }
}

/// A single C-ABI argument together with its register class.
///
/// The register class decides whether the 64-bit `bits` value is passed in a
/// general-purpose register (integers, pointers, JNI handles) or an SSE/XMM
/// register (float/double). This distinction is invisible once a value is
/// flattened to a bare `u64`, which is why the previous implementation passed
/// floats in integer registers — correct only on the Windows x64 ABI, wrong on
/// System V (Linux/macOS) where FP args use XMM0–XMM7.
#[derive(Clone, Copy)]
struct JniArg {
    bits: u64,
    /// `true` if this argument is floating-point (SSE register class).
    is_fp: bool,
}

impl JniArg {
    #[inline]
    fn int(bits: u64) -> Self {
        JniArg { bits, is_fp: false }
    }
    #[inline]
    fn float(bits: u64) -> Self {
        JniArg { bits, is_fp: true }
    }
}

/// Validate a JNI native function pointer before calling through it.
/// Returns `false` (and logs) for a null pointer, and for an entry address the
/// TARGET ISA cannot execute.
///
/// "Cannot execute" is an architecture fact, not a style preference, and the
/// two are not interchangeable. x86-64 instructions have NO alignment
/// requirement at all: a function may legally start on an odd byte, and gcc
/// does exactly that for small leaf functions — `cc -shared -fPIC -O1` on the
/// strict corpus’s own JNI fixture put `add` at `0x11f7`, `mulLong` at
/// `0x11ff` and `scale` at `0x120b`. The `fn_ptr % 2 != 0` check this function
/// used to apply therefore REFUSED seven of the twelve natives in that library
/// and returned 0 for each, which the Java side read as `add(40,2) == 0` —
/// silent data corruption produced by a guard, from a correctly resolved
/// pointer into correctly compiled code. Measured 2026-08-06 against the
/// HotSpot 25 arm binding the same `.so`.
///
/// aarch64 is different and keeps its check: A64 instructions are fixed-width
/// and must be 4-byte aligned, so an unaligned entry there really would fault.
#[inline]
fn jni_fn_ptr_ok(fn_ptr: usize) -> bool {
    if fn_ptr == 0 {
        tracing::error!("JNI call with null function pointer — returning 0");
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    if fn_ptr % 4 != 0 {
        tracing::error!(
            "JNI call with misaligned function pointer {:#x} — returning 0",
            fn_ptr
        );
        return false;
    }
    true
}

/// Call a JNI native function pointer, marshalling each argument into the
/// correct register class (GP vs XMM) and spilling any overflow to the stack,
/// per the platform calling convention. `args` already includes `env` and the
/// receiver/class as its first two (integer) entries.
///
/// On `x86_64` this routes through a small assembly trampoline that honours the
/// active ABI (Windows x64 or System V), so float/double arguments land in XMM
/// registers and calls with more than the register-resident argument count
/// correctly spill the remainder to the stack. When `fp_return` is set, the
/// XMM0 result is returned in place of RAX so the caller can reinterpret it as
/// a float/double.
///
/// On non-`x86_64` targets we keep the previous fixed-arity integer fast path
/// (correct for integer/reference args up to the register limit) and fail loud
/// for the float/double or large-arity cases we cannot honour without an
/// architecture-specific trampoline — never fabricating a 0/null result.
///
/// # Safety
/// `fn_ptr` must be a valid JNI native function whose C signature matches the
/// argument register classes described by `args` and the requested return kind.
unsafe fn call_jni_marshalled(fn_ptr: usize, args: &[JniArg], fp_return: bool) -> u64 {
    if !jni_fn_ptr_ok(fn_ptr) {
        return 0;
    }

    // Fast path: integer/reference-only calls (no FP arg, no FP return) are
    // dispatched with the proven fixed-arity `extern "C"` transmutes. The
    // compiler emits the correct ABI sequence for these, INCLUDING spilling any
    // integer overflow past the register file to the stack, so this is valid for
    // every all-integer arity the helper enumerates (up to MAX_INT_FAST total
    // args). This keeps the overwhelmingly common JNI call shape on the
    // well-tested path and confines the assembly trampoline strictly to the
    // cases that path cannot express: float/double arguments or return values.
    const MAX_INT_FAST: usize = 8; // highest fixed-arity arm in call_jni_fn_ptr_int
    if !fp_return && args.len() <= MAX_INT_FAST && !args.iter().any(|a| a.is_fp) {
        return call_jni_fn_ptr_int(fn_ptr, args);
    }

    #[cfg(target_arch = "x86_64")]
    {
        // Partition the arguments into GP-register, XMM-register and stack-spill
        // slots according to the active x86-64 ABI.
        //
        //  * Windows x64: the first FOUR arguments go in registers and integer
        //    vs FP shares one positional counter — i.e. argument N (0-based) uses
        //    GP slot N if integer or XMM slot N if FP, and any argument with
        //    N >= 4 spills to the stack. Each register arg still reserves its
        //    32-byte "shadow space" slot, which the trampoline allocates.
        //  * System V: integers and FP have INDEPENDENT counters — up to 6 GP
        //    registers (RDI,RSI,RDX,RCX,R8,R9) and up to 8 XMM registers
        //    (XMM0–7); anything beyond a class's register budget spills.
        let mut gp: [u64; 6] = [0; 6];
        let mut xmm: [u64; 8] = [0; 8];
        let mut stack: Vec<u64> = Vec::new();

        #[cfg(target_os = "windows")]
        {
            const MAX_REG: usize = 4; // RCX/RDX/R8/R9 share positions with XMM0–3
            let mut n_gp = 0usize;
            let mut n_xmm = 0usize;
            for (pos, a) in args.iter().enumerate() {
                if pos < MAX_REG {
                    if a.is_fp {
                        xmm[pos] = a.bits;
                        n_xmm = n_xmm.max(pos + 1);
                    } else {
                        gp[pos] = a.bits;
                        n_gp = n_gp.max(pos + 1);
                    }
                } else {
                    stack.push(a.bits);
                }
            }
            jni_trampoline_call(fn_ptr, &gp, n_gp, &xmm, n_xmm, &stack, fp_return)
        }

        #[cfg(not(target_os = "windows"))]
        {
            let mut n_gp = 0usize;
            let mut n_xmm = 0usize;
            for a in args.iter() {
                if a.is_fp {
                    if n_xmm < xmm.len() {
                        xmm[n_xmm] = a.bits;
                        n_xmm += 1;
                    } else {
                        stack.push(a.bits);
                    }
                } else if n_gp < gp.len() {
                    gp[n_gp] = a.bits;
                    n_gp += 1;
                } else {
                    stack.push(a.bits);
                }
            }
            jni_trampoline_call(fn_ptr, &gp, n_gp, &xmm, n_xmm, &stack, fp_return)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        // No architecture-specific trampoline. We can still correctly dispatch
        // integer/reference-only calls that fit the GP register file via fixed
        // -arity transmutes; FP args or oversized arg lists cannot be honoured
        // safely here, so we fail loud rather than fabricate a result.
        if args.iter().any(|a| a.is_fp) || fp_return {
            tracing::error!(
                "JNI native dispatch with float/double argument or return type is \
                 unsupported on this architecture (no FP-aware trampoline) — \
                 refusing to call to avoid an ABI mismatch"
            );
            jni_throw_unsatisfied_link(
                "float/double JNI signatures require an FP-aware trampoline on this architecture",
            );
            return 0;
        }
        call_jni_fn_ptr_int(fn_ptr, args)
    }
}

/// x86-64 assembly trampoline driver.
///
/// Lays out the GP registers, XMM registers and any stack-spill words into a
/// single contiguous control block and tail-calls the naked trampoline, which
/// performs the actual register loads and the `call`. The naked trampoline
/// stores XMM0 back into the control block on return so we can surface an
/// FP result.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn jni_trampoline_call(
    fn_ptr: usize,
    gp: &[u64; 6],
    n_gp: usize,
    xmm: &[u64; 8],
    n_xmm: usize,
    stack: &[u64],
    fp_return: bool,
) -> u64 {
    // Control block consumed by the naked trampoline. Field order/offsets are
    // mirrored exactly by the assembly below — DO NOT reorder without updating
    // the offsets there.
    #[repr(C)]
    struct CallBlock {
        fn_ptr: u64,    // +0
        gp: [u64; 6],   // +8
        xmm: [u64; 8],  // +56
        stack_ptr: u64, // +120  (pointer to first stack word, or null)
        n_stack: u64,   // +128  (count of stack words)
        n_xmm: u64,     // +136  (used for AL: # of vector regs, SysV varargs)
        xmm0_ret: u64,  // +144  (out: XMM0 result)
    }

    let mut block = CallBlock {
        fn_ptr: fn_ptr as u64,
        gp: *gp,
        xmm: *xmm,
        stack_ptr: if stack.is_empty() {
            0
        } else {
            stack.as_ptr() as u64
        },
        n_stack: stack.len() as u64,
        n_xmm: n_xmm as u64,
        xmm0_ret: 0,
    };
    let _ = n_gp; // GP regs are always loaded (unused entries are 0); kept for clarity.

    let rax = jni_naked_trampoline(&mut block as *mut CallBlock as *mut u8);
    if fp_return {
        block.xmm0_ret
    } else {
        rax
    }
}

// Naked x86-64 trampoline. Receives a single pointer to the `CallBlock` in the
// first integer-argument register of the active ABI (RCX on Windows, RDI on
// System V). It:
//   1. computes the stack space for spilled args (+ 32 bytes shadow space on
//      Windows), keeping 16-byte alignment at the `call`,
//   2. copies the stack-spill words into place,
//   3. loads the GP and XMM argument registers from the block,
//   4. sets AL = n_xmm (required by the System V variadic convention; ignored
//      by the Windows ABI),
//   5. `call`s the target, then stores XMM0 into the block and returns RAX.
//
// Field offsets MUST match `CallBlock` above. Two fully separate, item-level
// `cfg`-gated `global_asm!` blocks are used (rather than per-line `cfg`
// attributes inside one `global_asm!`, which are not supported) so the Windows
// x64 and System V variants are each unambiguous.
//
// Common prologue/epilogue note: after `push rbp; mov rbp,rsp; push rbx; push
// r12`, the saved-register window is [rbp]=rbp, [rbp-8]=rbx, [rbp-16]=r12, so
// the epilogue restores RSP with `lea rsp,[rbp-16]` before popping r12/rbx/rbp.

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
core::arch::global_asm!(
    ".p2align 4",
    ".globl jni_naked_trampoline_asm",
    "jni_naked_trampoline_asm:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "mov rbx, rcx",         // block ptr (Win64 arg0 = RCX)
    "mov r12, [rbx + 128]", // r12 = n_stack
    "mov rax, r12",
    "shl rax, 3",  // bytes for stack args
    "add rax, 32", // + 32-byte shadow space (Win64)
    "sub rsp, rax",
    "and rsp, -16",        // 16-byte align at the call
    "mov r8, [rbx + 120]", // r8 = stack_ptr (source, may be 0)
    "test r12, r12",
    "jz 2f",
    "test r8, r8",
    "jz 2f",
    "mov r9, rsp",
    "add r9, 32", // dest = above the shadow space
    "xor r10, r10",
    "1:",
    "mov rax, [r8 + r10*8]",
    "mov [r9 + r10*8], rax",
    "inc r10",
    "cmp r10, r12",
    "jb 1b",
    "2:",
    "movq xmm0, [rbx + 56]",
    "movq xmm1, [rbx + 64]",
    "movq xmm2, [rbx + 72]",
    "movq xmm3, [rbx + 80]",
    // XMM4–7 are loaded too (harmless on Win64; only XMM0–3 are arg regs).
    "movq xmm4, [rbx + 88]",
    "movq xmm5, [rbx + 96]",
    "movq xmm6, [rbx + 104]",
    "movq xmm7, [rbx + 112]",
    "mov rax, [rbx + 136]", // AL = n_xmm (ignored by Win64; set for uniformity)
    "mov r11, [rbx + 0]",   // target fn ptr
    "mov rcx, [rbx + 8]",   // gp[0]
    "mov rdx, [rbx + 16]",  // gp[1]
    "mov r8,  [rbx + 24]",  // gp[2]
    "mov r9,  [rbx + 32]",  // gp[3]
    "call r11",
    "movq [rbx + 144], xmm0", // capture FP return
    "lea rsp, [rbp - 16]",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
core::arch::global_asm!(
    ".p2align 4",
    ".globl jni_naked_trampoline_asm",
    "jni_naked_trampoline_asm:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "mov rbx, rdi",         // block ptr (SysV arg0 = RDI)
    "mov r12, [rbx + 128]", // r12 = n_stack
    "mov rax, r12",
    "shl rax, 3", // bytes for stack args (no shadow space on SysV)
    "sub rsp, rax",
    "and rsp, -16",        // 16-byte align at the call
    "mov r8, [rbx + 120]", // r8 = stack_ptr (source, may be 0)
    "test r12, r12",
    "jz 2f",
    "test r8, r8",
    "jz 2f",
    "mov r9, rsp", // dest = [rsp] (no shadow space)
    "xor r10, r10",
    "1:",
    "mov rax, [r8 + r10*8]",
    "mov [r9 + r10*8], rax",
    "inc r10",
    "cmp r10, r12",
    "jb 1b",
    "2:",
    "movq xmm0, [rbx + 56]",
    "movq xmm1, [rbx + 64]",
    "movq xmm2, [rbx + 72]",
    "movq xmm3, [rbx + 80]",
    "movq xmm4, [rbx + 88]",
    "movq xmm5, [rbx + 96]",
    "movq xmm6, [rbx + 104]",
    "movq xmm7, [rbx + 112]",
    "mov rax, [rbx + 136]", // AL = n_xmm (SysV variadic FP-reg count)
    "mov r11, [rbx + 0]",   // target fn ptr
    "mov rdi, [rbx + 8]",   // gp[0]
    "mov rsi, [rbx + 16]",  // gp[1]
    "mov rdx, [rbx + 24]",  // gp[2]
    "mov rcx, [rbx + 32]",  // gp[3]
    "mov r8,  [rbx + 40]",  // gp[4]
    "mov r9,  [rbx + 48]",  // gp[5]
    "call r11",
    "movq [rbx + 144], xmm0", // capture FP return
    "lea rsp, [rbp - 16]",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

#[cfg(target_arch = "x86_64")]
extern "C" {
    // The naked trampoline defined in `global_asm!` above. Takes the control
    // block pointer, returns the integer (RAX) result.
    fn jni_naked_trampoline_asm(block: *mut u8) -> u64;
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn jni_naked_trampoline(block: *mut u8) -> u64 {
    jni_naked_trampoline_asm(block)
}

/// Integer/reference-only dispatch via fixed-arity `extern "C"` transmutes, so
/// the compiler emits the correct platform register layout. This is the fast
/// path on `x86_64` for all-integer calls that fit in GP registers, and the
/// only dispatch mechanism on non-`x86_64` targets (which lack the assembly
/// trampoline). Calls with more arguments than the register file can hold are
/// not supported here and fail loud (UnsatisfiedLinkError) rather than silently
/// returning 0.
///
/// # Safety
/// Every entry of `args` must be an integer/reference-class value; `fn_ptr`
/// must match a signature receiving them all as 64-bit integers.
unsafe fn call_jni_fn_ptr_int(fn_ptr: usize, args: &[JniArg]) -> u64 {
    // args[0]=env, args[1]=recv, args[2..]=the Java params.
    let g = |i: usize| args[i].bits;
    match args.len() {
        0 => {
            let f: extern "C" fn() -> u64 = std::mem::transmute(fn_ptr);
            f()
        }
        1 => {
            let f: extern "C" fn(u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0))
        }
        2 => {
            let f: extern "C" fn(u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1))
        }
        3 => {
            let f: extern "C" fn(u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2))
        }
        4 => {
            let f: extern "C" fn(u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3))
        }
        5 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4))
        }
        6 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4), g(5))
        }
        7 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4), g(5), g(6))
        }
        8 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4), g(5), g(6), g(7))
        }
        n => {
            tracing::error!(
                "JNI call with {} total args exceeds the supported register count on this \
                 architecture (no stack-spill trampoline) — raising UnsatisfiedLinkError",
                n
            );
            jni_throw_unsatisfied_link(
                "JNI signatures with more register-resident integer arguments than this \
                 architecture supports require a stack-spilling trampoline",
            );
            0
        }
    }
}

/// Raise an `UnsatisfiedLinkError` on the current thread so an unsupported
/// native dispatch surfaces as a real Java exception rather than a fabricated
/// 0/null return.
///
/// Mirrors [`raise_jni_aioobe`]: when a `JvmThread` context is available we
/// materialise a real exception object and store its handle in
/// `JNI_PENDING_EXCEPTION`, so `vm_exec` rethrows it on return from the native
/// call. Without a thread context (e.g. a direct unit-test call) we fall back
/// to the `ThrowNew` sentinel so the error is flagged rather than swallowed.
fn jni_throw_unsatisfied_link(msg: &str) {
    let full = format!("Unsupported native method ABI: {msg}");
    let raised =
        with_jni_context(
            |shared, thread| match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/UnsatisfiedLinkError",
                Some(&full),
            ) {
                Ok(exc) => {
                    set_jni_pending_exception_object(exc);
                    Ok(())
                }
                Err(e) => Err(is_heap_exhaustion(&e)),
            },
        )
        .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        pend_unbuilt_throwable(heap_exhausted);
    }
}

// ---- Index 215: RegisterNatives ----

extern "C" fn jni_register_natives(
    _env: JNIEnv,
    clazz: JClass,
    methods: *const JNINativeMethod,
    n_methods: JInt,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if methods.is_null() || n_methods <= 0 || clazz == 0 {
        return JNI_OK;
    }

    // A `JClass` in this table IS a `ClassId` — that is what `FindClass`
    // returns (`class_id_to_jclass`, a `ClassId` in the low 32 bits under
    // `JCLASS_TAG`) and what GetSuperclass, IsAssignableFrom, GetFieldID,
    // CallStaticXxxMethod and a dozen others decode with
    // `ClassId::new(clazz as u32)` — the truncation drops the tag.
    //
    // This function decoded it as an OBJECT HANDLE instead, which is the one
    // convention `FindClass` never produces. Every odd-numbered ClassId took
    // the global-ref branch of `jobject_to_obj` and logged "handle 0x2cb not
    // found in global ref table"; every even one failed the heap-address
    // check. Either way `class_name` was None and RegisterNatives returned
    // JNI_ERR — so the ENTIRE RegisterNatives path was dead for the canonical
    // `FindClass` + `RegisterNatives` idiom that every JNI_OnLoad uses. It was
    // invisible because the library still loads, the symbol-bound natives
    // still resolve by name, and only the methods a library binds
    // exclusively through RegisterNatives raise UnsatisfiedLinkError.
    // Resolved BEFORE the class-manager read: `jclass_class_id` may take the
    // mirror-reverse lock, and nesting that inside a `class_manager` reader is
    // a lock order this file does not otherwise establish.
    let class_id = jclass_class_id(clazz);
    let class_name = with_shared_vm(|shared| {
        shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
    })
    .flatten();

    let class_name = match class_name {
        Some(n) => n,
        None => return JNI_ERR,
    };

    for i in 0..n_methods as usize {
        // Safety: caller guarantees `methods` points to `n_methods` valid elements
        let m = unsafe { &*methods.add(i) };
        if m.name.is_null() || m.signature.is_null() || m.fn_ptr.is_null() {
            continue;
        }
        // Safety: name and signature are null-terminated C strings from the JNI caller
        let name = unsafe { CStr::from_ptr(m.name).to_string_lossy().into_owned() };
        let sig = unsafe { CStr::from_ptr(m.signature).to_string_lossy().into_owned() };
        let fn_ptr = m.fn_ptr as usize;
        // Bind into the CALLING VM's table. `with_shared_vm` is the same handle
        // the class name above was read through, so a `RegisterNatives` driven
        // by a library another VM in this process loaded cannot land here.
        with_shared_vm(|shared| {
            register_jni_native(&shared.natives, &class_name, &name, &sig, fn_ptr);
        });
    }
    JNI_OK
}

// ---- Index 216: UnregisterNatives ----

extern "C" fn jni_unregister_natives(_env: JNIEnv, clazz: JClass) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return JNI_OK;
    }
    // Resolve the class name and remove all registered natives for it.
    // Since JNI_NATIVE_METHODS is keyed by hash, we need the class name
    // to reconstruct the keys. If we can't resolve the class, best-effort no-op.
    //
    // `JClass` is a `ClassId` here, matching `jni_register_natives` above and
    // the rest of the table. Decoding it as an object handle made this a
    // permanent no-op for anything `FindClass` returned, which paired with the
    // same defect in RegisterNatives: neither half of the pair worked.
    let class_info = with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| (class_id, c.name.clone()))
    })
    .flatten();

    if let Some((class_id, class_name)) = class_info {
        // Get all methods for this class and remove their native registrations
        let methods_to_remove: Vec<u64> = with_shared_vm(|shared| {
            let cm = shared.classes.class_manager.read();
            let mut keys = Vec::new();
            // `clazz` already supplied the exact loader-qualified ClassId.
            if let Some(class) = cm.get_class(class_id) {
                for method in &class.methods {
                    if method.is_native() {
                        let key = jni_native_key(&class_name, &method.name, &method.descriptor);
                        keys.push(key);
                    }
                }
            }
            keys
        })
        .unwrap_or_default();

        if !methods_to_remove.is_empty() {
            // Unbind from the CALLING VM's table only — the mirror image of
            // `RegisterNatives` above. While this was a process global, an
            // `UnregisterNatives` in one VM tore down another VM's bindings for
            // the same class.
            with_shared_vm(|shared| {
                let mut table = shared.natives.jni_native_methods.write();
                for key in &methods_to_remove {
                    table.remove(key);
                }
                drop(table);
                // gce e1/j: void every thread's `find_jni_native` memo.
                shared
                    .natives
                    .jni_native_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
            });
            tracing::debug!(
                "UnregisterNatives: removed {} native methods for {}",
                methods_to_remove.len(),
                class_name
            );
        }
    }
    JNI_OK
}

// ---------------------------------------------------------------------------
// Missing JNI functions (Phase 2 additions)
// ---------------------------------------------------------------------------

// ---- Index 28: AllocObject ----
// Allocates a new Java object without calling any constructor.
extern "C" fn jni_alloc_object(_env: JNIEnv, clazz: JClass) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        // `clazz` must resolve to a real class. If it does not (stale or
        // bogus handle), return null rather than allocating an object against
        // an unresolved/`ClassId(0)` class: a wrongly-classed object whose
        // declared field count is unknown corrupts every later field access.
        let num_fields = match shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
        {
            Some(n) => n,
            None => {
                tracing::warn!(
                    target: "cratonvm::jni",
                    clazz,
                    "AllocObject: JClass does not resolve to a loaded class"
                );
                return 0;
            }
        };
        let Some(obj) = jni_alloc_or_oom(shared, "an object (AllocObject)", |heap| {
            heap.try_alloc_object_full(class_id, num_fields)
        }) else {
            return 0;
        };
        // Typed field defaults, as a bytecode `new` and `Unsafe.allocateInstance`
        // (`NativeContextImpl::allocate_instance`) give them; AllocObject left
        // the allocator's raw slots (gc-common w4-g). No finalizer
        // registration: no constructor runs (see
        // `register_if_constructed_finalizable`).
        let _ = crate::runtime::interpreter::init_primitive_fields(shared, obj, class_id);
        new_local_handle(obj)
    })
    .unwrap_or(0)
}

/// Register `obj` for finalization if `class_id` overrides `finalize()`, for an
/// instance whose constructor is about to run. Returns whether it registered.
///
/// gc-common w4-g, the JNI half of
/// `handoff-w2d-finalizer-registration-native` (page
/// `common-d-finalizable-objects-from-native-allocation-paths-are-never-registered`).
/// HotSpot registers a finalizable instance when its `Object.<init>` returns
/// (`RegisterFinalizersAtInit`), so JNI `NewObject` / `NewObjectV` /
/// `NewObjectA` register and `AllocObject` -- no constructor -- does not. This
/// VM registers at allocation (`gc_and_alloc::init_new_instance`), so the one
/// JNI site is `NewObjectA` (the other two funnel into it), right after
/// `AllocObject`'s allocation and before `<init>`: the same point the
/// interpreter's `new` registers at. Exactly one registration: `AllocObject`
/// does not register, and nothing else registers a JNI-minted object.
fn register_if_constructed_finalizable(
    shared: &SharedVm,
    obj: ObjectRef,
    class_id: ClassId,
) -> bool {
    let has_finalizer = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_finalizer {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
    }
    has_finalizer
}

// ---- Index 30: NewObjectA ----
// Allocates a new Java object and invokes the constructor indicated by `mid`.
extern "C" fn jni_new_object_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || mid == 0 {
        return 0;
    }
    // Become a counted mutator for the whole alloc+<init> on a foreign thread
    // (the alloc itself touches the heap, so it must run as a mutator, not while
    // idle/blocked). Inert for non-foreign threads.
    let _fg = ForeignCallGuard::enter();
    // First allocate the object.
    let obj_handle = jni_alloc_object(_env, clazz);
    if obj_handle == 0 {
        return 0;
    }
    // Where `jni_alloc_object` recorded the handle (gc-common w3-g). `<init>`
    // may allocate and a MOVING collection may relocate the new object; the
    // recorded slot is rewritten by `update_local_refs_after_gc`, `obj_handle`
    // is not. Returning `obj_handle` handed the native the from-space address
    // on exactly that path.
    let slot = recorded_local_slot_of(obj_handle);
    // Then call the constructor (<init>) on the allocated object.
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj_handle)?;
        // JLS §12.6: a finalizable instance whose constructor runs is
        // registered; `AllocObject` alone (above) must not register.
        register_if_constructed_finalizable(shared, oref, jclass_class_id(clazz));
        let (decl_class_id, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.classes.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(&shared.mem.heap, args, &param_types) });
        // JDK-only §7: the `<init>` dispatch decision is taken inside
        // `invoke_on_class_shared`. The constructor's return value is `void` and
        // has always been discarded; only a JDK-only refusal is surfaced, so the
        // handle is still returned exactly as before and `Compatible` mode is
        // untouched. The caller sees the pending exception via
        // `ExceptionCheck`/`ExceptionOccurred`.
        let result = invoke_on_class_shared(
            shared,
            thread,
            jclass_class_id(clazz),
            &method_name,
            &descriptor,
            &jvm_args,
        );
        let _ = jni_surface_jdk_only(shared, thread, result);
        // The object's CURRENT address if a collection moved it during
        // `<init>`; `obj_handle` when there was no open frame to record it in.
        Some(slot.and_then(recorded_local_at).unwrap_or(obj_handle))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Indices 155-158: SetStaticBoolean/Byte/Char/ShortField ----
extern "C" fn jni_set_static_boolean_field(
    _env: JNIEnv,
    _clazz: JClass,
    fid: JFieldID,
    val: JBoolean,
) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_byte_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JByte) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_char_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JChar) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_short_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JShort) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

// ---- Index 163: NewString (UTF-16) ----
// Creates a java.lang.String from a UTF-16 char array.
extern "C" fn jni_new_string(_env: JNIEnv, unicode: *const JChar, len: JSize) -> JString {
    // gcd d10/j: as `jni_new_string_utf` (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if unicode.is_null() || len < 0 {
        return 0;
    }
    let chars: &[u16] = unsafe { std::slice::from_raw_parts(unicode, len as usize) };
    // gc-common w7-c: a NEW, un-interned String built from the exact UTF-16
    // units, allocated fallibly; see `jni_new_string_utf` for the leak and
    // identity defects of the interned form. Building from the units also
    // keeps a lone surrogate, which the old `from_utf16_lossy` replaced with
    // U+FFFD.
    with_shared_vm(|shared| {
        match jni_alloc_or_oom(shared, "a java/lang/String", |_| {
            crate::vm::try_create_java_string_from_units(shared, chars)
        }) {
            Some(obj) => new_local_handle(obj),
            None => 0,
        }
    })
    .unwrap_or(0)
}

// ---- Index 164: GetStringLength ----
extern "C" fn jni_get_string_length(_env: JNIEnv, str_obj: JString) -> JSize {
    // gcd d5/f: a leaf, as `GetStringUTFLength`.
    let _fx = ForeignJniEntry::enter_leaf(str_obj);
    if str_obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        // Java String length is the UTF-16 code unit count.
        Some(s.chars().map(|c| c.len_utf16()).sum::<usize>() as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 165/166: GetStringChars / ReleaseStringChars (UTF-16) ----
extern "C" fn jni_get_string_chars(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if str_obj == 0 {
        return std::ptr::null();
    }
    let result = with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        // Encode as UTF-16 and heap-allocate the buffer.
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let len = utf16.len();
        let boxed = utf16.into_boxed_slice();
        let ptr = boxed.as_ptr();
        std::mem::forget(boxed); // OWNERSHIP: buffer transferred to native caller, freed by jni_release_string_chars via Vec::from_raw_parts

        // Track the allocation so ReleaseStringChars can reconstruct the
        // correct Vec layout (pointer + length) for deallocation. In the VM's
        // table, so a release on any thread finds it (gc-common w10-c). An
        // empty string's buffer allocates nothing: its record (0 units) frees
        // nothing either.
        shared
            .natives
            .jni_global_refs
            .lock()
            .open_string_copy(ptr as usize, len);
        Some(ptr)
    })
    .flatten();
    match result {
        Some(ptr) => {
            if !is_copy.is_null() {
                unsafe {
                    *is_copy = JNI_TRUE;
                }
            }
            ptr
        }
        None => std::ptr::null(),
    }
}

extern "C" fn jni_release_string_chars(_env: JNIEnv, _str: JString, chars: *const JChar) {
    if !chars.is_null() {
        // Look up the original element count so we can reconstruct the Vec
        // with the correct layout.  Without the length the global allocator
        // would receive a mismatched dealloc (single-element vs slice).
        let len = with_shared_vm(|shared| {
            shared
                .natives
                .jni_global_refs
                .lock()
                .take_string_copy(chars as usize)
        })
        .flatten()
        .unwrap_or(0);
        if len > 0 {
            unsafe {
                drop(Vec::from_raw_parts(chars as *mut JChar, len, len));
            }
        }
        // len == 0 means the pointer wasn't tracked (shouldn't happen in
        // correct usage).  We intentionally leak rather than corrupt the
        // allocator with a wrong layout.
    }
}

// ---------------------------------------------------------------------------
// va_list helpers — extract JNI arguments from a C va_list pointer
// ---------------------------------------------------------------------------
//
// On x86-64 Windows the calling convention passes a va_list as a simple
// pointer to an 8-byte-aligned argument block.  Each slot is 8 bytes wide
// regardless of the actual argument type.
//
// On x86-64 SysV (Linux/macOS) va_list is a *struct { gp_offset, fp_offset,
// overflow_arg_area, reg_save_area }*, which is considerably more complex.
// For portability we define both paths and use cfg to select.

/// Opaque C va_list pointer.
pub type VaList = *mut u8;

/// A walk over a C `va_list`, in whichever shape the target ABI gives it.
///
/// This is not a detail that can be papered over with "8-byte slots". On
/// **System V x86-64** a `va_list` is a four-field struct — two offsets, an
/// overflow pointer and a register-save area — because the first six integer
/// and first eight FP arguments were never on the stack: they live in a save
/// area the variadic callee spilled them into, and the two classes advance
/// through it INDEPENDENTLY. Reading that struct as an array of arguments
/// decodes its own `gp_offset`/`fp_offset` header as argument one.
///
/// On **Windows x64** (and Apple's arm64) a `va_list` really is a bare pointer
/// into a contiguous run of 8-byte slots, which is the shape this code
/// previously assumed on every platform.
enum VaCursor {
    /// Contiguous 8-byte slots: Windows x64, Apple arm64.
    Flat { p: *const u8 },
    /// System V x86-64: a pointer to one `__va_list_tag`.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    SysV { tag: *mut SysVVaListTag },
}

/// The System V x86-64 `__va_list_tag`. `va_list` is a one-element array of
/// it, so a `va_list` argument decays to a pointer to this.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
#[repr(C)]
struct SysVVaListTag {
    /// Byte offset of the next unconsumed INTEGER register slot, 0..48.
    gp_offset: u32,
    /// Byte offset of the next unconsumed SSE register slot, 48..176.
    fp_offset: u32,
    /// Next stack-passed argument.
    overflow_arg_area: *mut u8,
    /// 6 GP slots of 8 bytes, then 8 SSE slots of 16 bytes = 176 bytes.
    reg_save_area: *mut u8,
}

/// End of the six integer register slots in a System V register-save area.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
const SYSV_GP_LIMIT: u32 = 6 * 8;
/// End of the eight SSE register slots in a System V register-save area.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
const SYSV_FP_LIMIT: u32 = 6 * 8 + 8 * 16;

impl VaCursor {
    fn new(va: VaList) -> Self {
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        {
            VaCursor::SysV {
                tag: va as *mut SysVVaListTag,
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(target_os = "windows"))))]
        {
            VaCursor::Flat { p: va as *const u8 }
        }
    }

    /// Consume one argument and return its raw 8 bytes. `is_fp` selects the
    /// SSE class (`float`/`double`); every other JNI argument type, references
    /// included, is INTEGER class.
    ///
    /// # Safety
    /// The cursor must be positioned on an argument the caller actually
    /// pushed, in the class the caller pushed it as.
    unsafe fn next(&mut self, is_fp: bool) -> u64 {
        match self {
            VaCursor::Flat { p } => {
                let v = *(*p as *const u64);
                *p = p.add(8);
                v
            }
            #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
            VaCursor::SysV { tag } => {
                let t = &mut **tag;
                if is_fp {
                    if t.fp_offset < SYSV_FP_LIMIT {
                        let p = t.reg_save_area.add(t.fp_offset as usize);
                        t.fp_offset += 16;
                        return *(p as *const u64);
                    }
                } else if t.gp_offset < SYSV_GP_LIMIT {
                    let p = t.reg_save_area.add(t.gp_offset as usize);
                    t.gp_offset += 8;
                    return *(p as *const u64);
                }
                // Spilled. Every overflow argument occupies exactly 8 bytes —
                // a `double` included; only 16-byte-aligned types differ, and
                // no JNI argument type is one.
                let p = t.overflow_arg_area;
                t.overflow_arg_area = t.overflow_arg_area.add(8);
                *(p as *const u64)
            }
        }
    }
}

/// Read a JNI method's arguments from a va_list and pack them into a JValue
/// array.  The caller is responsible for passing the correct `mid` so that we
/// can look up the method descriptor and determine argument types.
///
/// Returns `std::ptr::null()` on failure; otherwise returns a heap-allocated
/// JValue slice that the caller must free with `Box::from_raw`.
fn va_list_to_jvalues(mid: JMethodID, va: VaList) -> (*const JValue, usize) {
    if mid == 0 || va.is_null() {
        return (std::ptr::null(), 0);
    }
    // Look up the method descriptor to know argument types.
    let descriptor = with_shared_vm(|shared| {
        let (decl_class_id, method_index) = decode_method_id(mid);
        let cm = shared.classes.class_manager.read();
        let class = cm.class_store.get(decl_class_id)?;
        let method = class.methods.get(method_index as usize)?;
        Some(method.descriptor.clone())
    })
    .flatten();
    let descriptor = match descriptor {
        Some(d) => d,
        None => return (std::ptr::null(), 0),
    };
    let param_types = parse_param_types_cached(&descriptor);
    if param_types.is_empty() {
        return (std::ptr::null(), 0);
    }
    let mut jvalues: Vec<JValue> = Vec::with_capacity(param_types.len());
    let mut cursor = VaCursor::new(va);
    for &tag in &param_types {
        // C's default argument promotions apply to everything that reaches a
        // `...`, and therefore to everything that reaches the `va_list` form:
        // a `jfloat` argument arrives as a DOUBLE. Reading 'F' as raw 32 bits
        // — which this loop used to do — decoded half of a double's mantissa.
        let is_fp = tag == b'F' || tag == b'D';
        // SAFETY: `cursor` walks a live `va_list` the caller vouched for, and
        // `param_types` comes from the method's own descriptor, so it consumes
        // exactly the arguments the caller pushed, in their register classes.
        let raw: u64 = unsafe { cursor.next(is_fp) };
        let jv = match tag {
            b'Z' => JValue { z: raw as JBoolean },
            b'B' => JValue { b: raw as JByte },
            b'C' => JValue { c: raw as JChar },
            b'S' => JValue { s: raw as JShort },
            b'I' => JValue { i: raw as JInt },
            b'J' => JValue { j: raw as JLong },
            b'F' => JValue { f: f64::from_bits(raw) as f32 },
            b'D' => JValue { d: f64::from_bits(raw) },
            _ /* L, [ */ => JValue { l: raw },
        };
        jvalues.push(jv);
    }
    let len = jvalues.len();
    let boxed = jvalues.into_boxed_slice();
    let ptr = boxed.as_ptr();
    std::mem::forget(boxed); // OWNERSHIP: buffer transferred to caller, freed by free_jvalues via Box::from_raw
    (ptr, len)
}

/// Free a JValue array returned by `va_list_to_jvalues`.
unsafe fn free_jvalues(ptr: *const JValue, len: usize) {
    if !ptr.is_null() && len > 0 {
        // Rebuild the `Box<[JValue]>` that `va_list_to_jvalues` forgot.
        // `slice_from_raw_parts_mut` builds the fat pointer directly, instead
        // of materialising a `&mut [JValue]` and casting it back to a raw
        // pointer: the reference form asserts an exclusive borrow of memory
        // this function is about to hand to `Box::from_raw`, which is what
        // `clippy::cast_slice_from_raw_parts` (denied here) objects to. Same
        // pointer, same length, no intermediate reference.
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ptr as *mut JValue,
            len,
        )));
    }
}

// ---------------------------------------------------------------------------
// V-variant method call wrappers (take va_list instead of JValue*)
// ---------------------------------------------------------------------------

// --- NewObjectV (slot 29) ---
extern "C" fn jni_new_object_v(env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let result = jni_new_object_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    result
}

// --- Instance CallXxxMethodV (slots 35, 38, 41, 44, 47, 50, 53, 56, 59, 62) ---
extern "C" fn jni_call_object_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_object_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_boolean_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_boolean_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_byte_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_byte_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_char_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_char_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_short_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_short_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_int_method_v(env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_int_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_long_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_long_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_float_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_float_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_double_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_double_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_void_method_v(env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_void_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
}

// --- Nonvirtual CallNonvirtualXxxMethodV (slots 65, 68, 71, 74, 77, 80, 83, 86, 89, 92) ---
extern "C" fn jni_call_nonvirtual_object_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_object_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_boolean_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_boolean_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_byte_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_byte_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_char_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_char_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_short_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_short_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_int_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_int_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_long_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_long_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_float_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_float_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_double_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_double_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_nonvirtual_void_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_nonvirtual_void_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
}

// --- Static CallStaticXxxMethodV (slots 115, 118, 121, 124, 127, 130, 133, 136, 139, 142) ---
extern "C" fn jni_call_static_object_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_object_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_boolean_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_boolean_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_byte_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_byte_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_char_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_char_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_short_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_short_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_int_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_int_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_long_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_long_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_float_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_float_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_double_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_double_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

extern "C" fn jni_call_static_void_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_static_void_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
}

// ---------------------------------------------------------------------------
// NIO Direct ByteBuffer support (slots 229-231)
// ---------------------------------------------------------------------------
//
// A DirectByteBuffer's native address and capacity live in the buffer object's
// own fields — there is no side table. Which slots those are depends on the
// class we actually got:
//
//   * **real-JDK mode** — `java/nio/DirectByteBuffer` resolves to the real
//     class, so `address` (`J`) and `capacity` (`I`) are inherited from
//     `java.nio.Buffer` at their real layout slots. `dbb_slots` finds them by
//     name+descriptor and both the setter and the getters use those slots.
//   * **synthetic-jdk / stub mode** — the class is a fabricated stub with no
//     declared fields, so nothing resolves by name and we fall back to the
//     historical fixed slots 0 (address, `Long`) and 1 (capacity, `Long`),
//     which a stub object stores as tagged `Value`s and therefore round-trips
//     exactly.
//
// PROCESS-GLOBAL-STATE ROUND 2 — this replaces a `LazyLock<Mutex<HashMap<u64,
// (SendPtr, i64)>>>` side table keyed on `obj_to_jobject(obj)`, i.e. on the
// buffer's **raw heap address**. That table was never remapped after a moving
// collection, never swept when a buffer died, shared by every VM in the
// process, and consulted BEFORE the authoritative field read. Once the
// collector recycled a dead buffer's address for any new object, a
// `GetDirectBufferAddress` on that new object returned the *previous* buffer's
// `malloc` pointer — an arbitrary out-of-bounds native read/write reachable
// from ordinary Java code (Netty and Elasticsearch both drive this path).
//
// The table also masked a second bug: writing `Value::Long(address)` into slot
// 0 of a *real* `java.nio.Buffer` targets `mark` (an `int`), and
// `write_compact_field`'s `Int` arm stores 0 for a non-`Int` value — so in
// real-JDK mode the buffer handed back to Java had `address == 0`,
// `mark == 0` and `position == capacity`. Only the side table made the JNI
// getters appear to work. Writing by name fixes the object itself, which is
// what Java-side `ByteBuffer` operations read.

/// Resolve the `(address, capacity)` layout slots of a real `java.nio.Buffer`
/// hierarchy, or `None` when the class is a fabricated stub with no declared
/// fields (synthetic-jdk mode).
///
/// Descriptor-qualified so a subclass field that merely shares the name cannot
/// shadow `Buffer.address` / `Buffer.capacity`. Both slots must resolve — a
/// half-resolved layout is not a layout we can round-trip through, so the
/// caller falls back to the fixed-slot form for *both* values and the setter
/// and getters therefore always agree.
fn dbb_slots(shared: &SharedVm, class_id: ClassId) -> Option<(usize, usize)> {
    let cm = shared.classes.class_manager.read();
    let addr = crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
        class_id,
        "address",
        Some("J"),
        &cm.class_store,
    )?;
    let cap = crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
        class_id,
        "capacity",
        Some("I"),
        &cm.class_store,
    )?;
    Some((addr, cap))
}

/// Read a slot that may hold either an `I` or a `J` payload, widening to i64.
/// Returns `None` for any other `Value` (notably `Object(None)`, which is what
/// the heap's bounds guard returns for an out-of-range slot).
fn dbb_read_integral(shared: &SharedVm, obj: ObjectRef, index: usize) -> Option<i64> {
    match shared.mem.heap.get_field(obj, index) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(v as i64),
        _ => None,
    }
}

/// The capacity `GetDirectBufferCapacity` answers for this buffer.
///
/// Factored out because `GetDirectBufferAddress` now needs the SAME number to
/// bound the pointer it publishes (see `direct_buffer_native_address`). Two
/// copies of this resolution would be two chances for the pair to disagree,
/// which is precisely the failure the bound is there to prevent.
///
/// A capacity of 0 is legal (`NewDirectByteBuffer(addr, 0)`), so unlike the
/// address there is no "non-zero means present" test available. Resolution
/// decides: if the class has a real `capacity` field, that field is
/// authoritative; otherwise slot 1 is. The `or_else` covers the
/// stub-upgraded-under-a-live-object case, as in the address getter.
fn dbb_capacity(shared: &SharedVm, oref: ObjectRef, class_id: ClassId) -> Option<i64> {
    match dbb_slots(shared, class_id) {
        Some((_, cap_idx)) => {
            dbb_read_integral(shared, oref, cap_idx).or_else(|| dbb_read_integral(shared, oref, 1))
        }
        None => dbb_read_integral(shared, oref, 1),
    }
}

// Index 229: NewDirectByteBuffer
extern "C" fn jni_new_direct_byte_buffer(
    _env: JNIEnv,
    address: *mut u8,
    capacity: JLong,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if address.is_null() || capacity < 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        // Allocate a java/nio/DirectByteBuffer-like object with at least 2
        // fields. The object's class must declare those fields — allocating
        // with `ClassId::new(0)` (`java/lang/Object`, zero declared fields)
        // yields an undersized object that the GC's `get_field` bounds guard
        // rejects on every access.
        let Some(dbb_class_id) = jni_class_or_refuse(shared, "java/nio/DirectByteBuffer", 2) else {
            return 0;
        };
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .get_class(dbb_class_id)
            .map_or(2, |c| c.num_total_fields.max(2));
        let Some(obj) = jni_alloc_or_oom(shared, "a java.nio.DirectByteBuffer", |heap| {
            heap.try_alloc_object_full(dbb_class_id, num_fields)
        }) else {
            return 0;
        };
        let heap = &shared.mem.heap;
        match dbb_slots(shared, dbb_class_id) {
            Some((addr_idx, cap_idx)) => {
                // Real `java.nio.Buffer` layout. Seed the whole invariant set
                // (`mark`/`position`/`limit`/`capacity`), not just the two the
                // JNI getters read: the object is handed straight to Java, and
                // `java.nio.Buffer`'s own methods assume
                // `mark <= position <= limit <= capacity`. A default-zeroed
                // `limit` would make every `get`/`put` throw
                // `BufferUnderflow`/`BufferOverflow`.
                // Resolve every slot FIRST and release the class-manager read
                // lock before touching the heap: `set_field`'s diagnostic paths
                // can resolve class metadata themselves, and holding a reader
                // across them risks a writer-starvation stall.
                let extra: Vec<(usize, Value)> = {
                    let cm = shared.classes.class_manager.read();
                    [
                        ("limit", Value::Int(capacity as i32)),
                        ("position", Value::Int(0)),
                        ("mark", Value::Int(-1)),
                    ]
                    .into_iter()
                    .filter_map(|(name, value)| {
                        crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
                            dbb_class_id,
                            name,
                            Some("I"),
                            &cm.class_store,
                        )
                        .map(|idx| (idx, value))
                    })
                    .collect()
                };
                heap.set_field(obj, addr_idx, Value::Long(address as i64));
                heap.set_field(obj, cap_idx, Value::Int(capacity as i32));
                for (idx, value) in extra {
                    heap.set_field(obj, idx, value);
                }
            }
            None => {
                // Fabricated-stub layout: tagged `Value` slots, so a `Long`
                // round-trips verbatim. Historical fixed slots.
                heap.set_field(obj, 0, Value::Long(address as i64));
                heap.set_field(obj, 1, Value::Long(capacity));
            }
        }
        new_local_handle(obj)
    })
    .unwrap_or(0)
}

/// Turn the `long` a direct buffer carries in its `address` field into
/// something a native library can actually dereference.
///
/// `Unsafe.allocateMemory` on this VM does NOT return an OS pointer: it returns
/// a *handle* into a Rust-side arena, carrying `ARENA_TAG` (bit 62) precisely so
/// it can never be confused with a real pointer. `ByteBuffer.allocateDirect`
/// runs the real JDK's bytecode, which allocates through `Unsafe`, so every
/// direct buffer this VM hands to Java has a tagged handle in `Buffer.address`.
///
/// Inside the VM that is invisible — the `Unsafe` get/put natives route a tagged
/// address back to the arena. It stops being invisible at the JNI boundary,
/// because `GetDirectBufferAddress` returns a bare `void*` that the native
/// *itself* dereferences. lz4-java's `XXH32BB` is one line —
/// `XXH32(GetDirectBufferAddress(env, buf) + off, len, seed)` — and zstd-jni's
/// `getDirectByteBufferFrameContentSize` is another; returning a handle to
/// either is an immediate SIGSEGV in third-party C, with no CratonVM frame at
/// the fault to say why.
///
/// So a tagged handle is translated to the real address of the arena block's
/// backing bytes. Writes by the native land in the arena directly, which is the
/// aliasing a direct buffer is defined to have, and a subsequent `Unsafe` read
/// through the handle sees them. An untagged address is already a real pointer
/// (a native's own `NewDirectByteBuffer` allocation) and is passed through.
///
/// A tagged address that no *live* block contains — a use-after-free, or an
/// offset past the end — resolves to `None` and the caller answers JNI NULL.
/// NULL is what the JNI spec already reserves for "not a direct buffer", so
/// natives that check at all check for it; returning the raw handle instead
/// would trade a null check for a wild store.
/// Is this object a DIRECT buffer — the only kind `GetDirectBufferAddress` and
/// `GetDirectBufferCapacity` may answer for?
///
/// This used to be inferred from `Buffer.address != 0`, and that inference was
/// correct on JDK 8 and is WRONG on JDK 21+. `Buffer.address` was repurposed:
/// a heap buffer now carries `ARRAY_BYTE_BASE_OFFSET` there — literally `16` —
/// as the base for the `Unsafe` accesses that pair it with `hb`. Measured on
/// Temurin 25.0.3.9, `ByteBuffer.allocate(16).address == 16` on HotSpot AND on
/// CratonVM; the two agree about the field, so the divergence was entirely in
/// reading it as a pointer. `GetDirectBufferAddress(heapBuffer)` therefore
/// answered `(void*)16` where HotSpot answers NULL — a wild pointer handed to
/// any native that trusts it, which is the same crash family as the arena
/// handle this function's sibling comment describes, from a different cause.
///
/// The rule is the JDK's own: a direct buffer is one that implements
/// `sun.nio.ch.DirectBuffer`. The class-name check on `java/nio/DirectByteBuffer`
/// is checked alongside it because a buffer minted by `NewDirectByteBuffer`
/// under `--synthetic-jdk` has that class but need not have the interface.
fn is_direct_buffer(shared: &SharedVm, oref: ObjectRef) -> bool {
    // An array's header carries its COMPONENT's class id, so a
    // `DirectByteBuffer[]` would pass the walk below and have its elements read
    // as buffer fields.
    if shared.mem.heap.kind_of(oref) == crate::memory::heap::ObjectKind::Array {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    let mut current = shared.mem.heap.class_id_of(oref);
    // Bounded: a malformed or cyclic hierarchy must not spin inside a JNI call.
    for _ in 0..64 {
        let Some(class) = cm.get_class(current) else {
            return false;
        };
        if &*class.name == "java/nio/DirectByteBuffer"
            || &*class.name == "java/nio/MappedByteBuffer"
        {
            return true;
        }
        if class.interfaces.iter().any(|&i| {
            cm.get_class(i)
                .is_some_and(|c| &*c.name == "sun/nio/ch/DirectBuffer")
        }) {
            return true;
        }
        match class.superclass {
            Some(sc) => current = sc,
            None => return false,
        }
    }
    false
}

/// The real address `GetDirectBufferAddress` may publish for `raw`, given that
/// `GetDirectBufferCapacity` is about to answer `capacity` for the same buffer.
///
/// The two JNI entry points ARE the bound: the spec's contract is that a native
/// may touch `capacity` bytes starting at the address, so publishing them
/// separately without checking they agree hands out a pointer with a bound
/// nobody enforced. That is what
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` recorded as "the
/// handle TRANSLATED, and the bound dropped" — `unsafe_arena_real_ptr` knows
/// how many bytes are left in the block and this call site used to discard it.
///
/// A tagged handle whose block cannot cover `capacity` is refused, not clamped:
/// the capacity getter reads a Java field this function cannot correct, so the
/// only self-consistent pair on offer is `(NULL, capacity)` — and NULL is the
/// answer natives already check for.
fn direct_buffer_native_address(raw: i64, capacity: i64) -> Option<*mut u8> {
    if raw == 0 {
        return None;
    }
    if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(raw) {
        // A negative capacity is not a length; treat it as zero rather than
        // wrapping it into a colossal `usize` that refuses every buffer.
        let want = usize::try_from(capacity).unwrap_or(0);
        return cratonvm_native_builtins::unsafe_arena_real_ptr_bounded(raw, want);
    }
    Some(raw as *mut u8)
}

// Index 230: GetDirectBufferAddress
extern "C" fn jni_get_direct_buffer_address(_env: JNIEnv, buf: JObject) -> *mut u8 {
    let _fx = ForeignJniEntry::enter();
    if buf == 0 {
        return std::ptr::null_mut();
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        // Non-direct buffers answer NULL — see `is_direct_buffer` for why the
        // address field alone cannot decide this on JDK 21+.
        if !is_direct_buffer(shared, oref) {
            return None;
        }
        let class_id = shared.mem.heap.class_id_of(oref);
        // The same resolution the setter used, so setter and getter always
        // address the same slot. The `or_else` covers exactly one drift case:
        // the class was a stub when the buffer was allocated and has since been
        // upgraded to the real class, so the *object* is still stub-shaped and
        // the named slot reads out of bounds (`Object(None)` -> `None`). Only a
        // `None` falls through — a named slot that reads a real value is always
        // preferred, so a real `Buffer`'s `mark` can never be mistaken for an
        // address.
        // The capacity this buffer is about to advertise through
        // `GetDirectBufferCapacity`. A buffer that cannot answer one at all
        // gets 0, which bounds nothing away: an arena block always covers zero
        // bytes, so such a buffer behaves exactly as it did before the bound
        // was carried.
        let capacity = dbb_capacity(shared, oref, class_id).unwrap_or(0);
        match dbb_slots(shared, class_id) {
            Some((addr_idx, _)) => dbb_read_integral(shared, oref, addr_idx)
                .or_else(|| dbb_read_integral(shared, oref, 0)),
            None => dbb_read_integral(shared, oref, 0),
        }
        .and_then(|raw| direct_buffer_native_address(raw, capacity))
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// Index 231: GetDirectBufferCapacity
extern "C" fn jni_get_direct_buffer_capacity(_env: JNIEnv, buf: JObject) -> JLong {
    let _fx = ForeignJniEntry::enter();
    if buf == 0 {
        return -1;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        // -1 for a non-direct buffer, symmetric with the address getter. A heap
        // buffer has a perfectly good `capacity` field, so without this the
        // capacity said "yes, a direct buffer of N bytes" about the very object
        // the address getter refuses — which is worse than either answer alone.
        if !is_direct_buffer(shared, oref) {
            return None;
        }
        let class_id = shared.mem.heap.class_id_of(oref);
        dbb_capacity(shared, oref, class_id)
    })
    .flatten()
    .unwrap_or(-1)
}

// Index 232: GetObjectRefType (correct JNI 1.6 slot)
// Already implemented at index 19 (jni_get_object_ref_type) — we alias it here.

// Index 233: GetModule (JNI 9+)
// Returns the java.lang.Module that the class belongs to.
// For now, we return null (unnamed module) since our module system is basic.
extern "C" fn jni_get_module(_env: JNIEnv, _clazz: JClass) -> JObject {
    // All classes are in the unnamed module for now.
    0
}

// ---- Index 234: IsVirtualThread (JNI 21) ----
extern "C" fn jni_is_virtual_thread(env: JNIEnv, obj: JObject) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return JNI_FALSE;
    }
    let clazz = jni_get_object_class(env, obj);
    if clazz == 0 {
        return JNI_FALSE;
    }
    // Same predicate `vm_exec` uses to decide a thread is virtual: every
    // virtual thread implementation (`VirtualThread`, `ThreadBuilders$
    // BoundVirtualThread`) extends `java.lang.BaseVirtualThread`. If that
    // class was never loaded, no virtual thread exists yet and the answer is
    // `false` for every receiver.
    let base = with_shared_vm(|shared| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/BaseVirtualThread")
    })
    .flatten();
    match base {
        Some(b) => jni_is_assignable_from(env, clazz, class_id_to_jclass(b)),
        None => JNI_FALSE,
    }
}

// ---- Index 235: GetStringUTFLengthAsLong (JNI 24) ----
extern "C" fn jni_get_string_utf_length_as_long(env: JNIEnv, str_obj: JString) -> JLong {
    let _fx = ForeignJniEntry::enter();
    // Identical measurement to GetStringUTFLength, in the wider return type
    // that exists so a string longer than `jsize` can report its real modified
    // UTF-8 length instead of overflowing.
    if str_obj == 0 {
        return 0;
    }
    let _ = env;
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.mem.heap, oref)?;
        Some(modified_utf8_len(&s) as JLong)
    })
    .flatten()
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// JNI Function Table (flat array)
// ---------------------------------------------------------------------------

/// Build the JNI function table at runtime.
fn build_function_table() -> Box<[usize; JNI_FUNCTION_COUNT]> {
    let stub = jni_stub as *const () as usize;
    let mut t = [stub; JNI_FUNCTION_COUNT];

    // Version
    t[4] = jni_get_version as *const () as usize;

    // Class operations
    t[5] = jni_define_class as *const () as usize;
    t[6] = jni_find_class as *const () as usize;
    t[7] = jni_from_reflected_method as *const () as usize;
    t[8] = jni_from_reflected_field as *const () as usize;
    t[9] = jni_to_reflected_method as *const () as usize;
    t[10] = jni_get_superclass as *const () as usize;
    t[11] = jni_is_assignable_from as *const () as usize;
    t[12] = jni_to_reflected_field as *const () as usize;

    // Exceptions
    t[13] = jni_throw as *const () as usize;
    t[14] = jni_throw_new as *const () as usize;
    t[15] = jni_exception_occurred as *const () as usize;
    t[16] = jni_exception_describe as *const () as usize;
    t[17] = jni_exception_clear as *const () as usize;
    t[18] = jni_fatal_error as *const () as usize;

    // Local frame management.
    //
    // Slots 19..=27 used to sit one HIGHER than `jni.h` puts them, because a
    // second `GetObjectRefType` had been wired at 19 (its real slot is 232,
    // still wired below) and pushed everything after it along. A native does
    // not name these functions — it loads slot N and calls it — so the shift
    // was silent and total: `DeleteLocalRef` ran `DeleteGlobalRef`,
    // `NewGlobalRef` ran `PopLocalFrame` (popping a frame the VM still
    // believed was live), `DeleteGlobalRef` ran `NewGlobalRef` (so every
    // release leaked a fresh global root), and `IsSameObject` — the idiom
    // every library uses for `o == null` and for reference identity — ran the
    // `void` `DeleteLocalRef` and returned whatever was left in RAX.
    t[19] = jni_push_local_frame as *const () as usize;
    t[20] = jni_pop_local_frame as *const () as usize;

    // References
    t[21] = jni_new_global_ref as *const () as usize;
    t[22] = jni_delete_global_ref as *const () as usize;
    t[23] = jni_delete_local_ref as *const () as usize;
    t[24] = jni_is_same_object as *const () as usize;
    t[25] = jni_new_local_ref as *const () as usize;
    t[26] = jni_ensure_local_capacity as *const () as usize;
    t[27] = jni_alloc_object as *const () as usize;
    // t[28] = NewObject (bare varargs) — wired with the other `...` slots below.
    t[29] = jni_new_object_v as *const () as usize;
    t[30] = jni_new_object_a as *const () as usize;

    // Object operations
    t[31] = jni_get_object_class as *const () as usize;
    t[32] = jni_is_instance_of as *const () as usize;

    // Method IDs
    t[33] = jni_get_method_id as *const () as usize;

    // The bare-varargs `...` slots — NewObject (28), Call<T>Method (34,37,…),
    // CallNonvirtual<T>Method (64,67,…) and CallStatic<T>Method (114,117,…) —
    // go to the assembly trampolines above, which build the platform `va_list`
    // and hand off to the `V` sibling. On an architecture with no trampoline
    // they keep `jni_varargs_unsupported`, which raises UnsatisfiedLinkError
    // rather than fabricating a 0/null return.
    for (slot, fn_addr) in jni_varargs_slots() {
        t[slot] = fn_addr;
    }

    // Call<Type>Method/V/A — virtual instance (groups of 3: varargs, va_list, array)
    t[35] = jni_call_object_method_v as *const () as usize;
    t[36] = jni_call_object_method_a as *const () as usize;
    t[38] = jni_call_boolean_method_v as *const () as usize;
    t[39] = jni_call_boolean_method_a as *const () as usize;
    t[41] = jni_call_byte_method_v as *const () as usize;
    t[42] = jni_call_byte_method_a as *const () as usize;
    t[44] = jni_call_char_method_v as *const () as usize;
    t[45] = jni_call_char_method_a as *const () as usize;
    t[47] = jni_call_short_method_v as *const () as usize;
    t[48] = jni_call_short_method_a as *const () as usize;
    t[50] = jni_call_int_method_v as *const () as usize;
    t[51] = jni_call_int_method_a as *const () as usize;
    t[53] = jni_call_long_method_v as *const () as usize;
    t[54] = jni_call_long_method_a as *const () as usize;
    t[56] = jni_call_float_method_v as *const () as usize;
    t[57] = jni_call_float_method_a as *const () as usize;
    t[59] = jni_call_double_method_v as *const () as usize;
    t[60] = jni_call_double_method_a as *const () as usize;
    t[62] = jni_call_void_method_v as *const () as usize;
    t[63] = jni_call_void_method_a as *const () as usize;

    // CallNonvirtual<Type>Method/V/A (groups of 3); the `...` slots are wired above.
    t[65] = jni_call_nonvirtual_object_method_v as *const () as usize;
    t[66] = jni_call_nonvirtual_object_method_a as *const () as usize;
    t[68] = jni_call_nonvirtual_boolean_method_v as *const () as usize;
    t[69] = jni_call_nonvirtual_boolean_method_a as *const () as usize;
    t[71] = jni_call_nonvirtual_byte_method_v as *const () as usize;
    t[72] = jni_call_nonvirtual_byte_method_a as *const () as usize;
    t[74] = jni_call_nonvirtual_char_method_v as *const () as usize;
    t[75] = jni_call_nonvirtual_char_method_a as *const () as usize;
    t[77] = jni_call_nonvirtual_short_method_v as *const () as usize;
    t[78] = jni_call_nonvirtual_short_method_a as *const () as usize;
    t[80] = jni_call_nonvirtual_int_method_v as *const () as usize;
    t[81] = jni_call_nonvirtual_int_method_a as *const () as usize;
    t[83] = jni_call_nonvirtual_long_method_v as *const () as usize;
    t[84] = jni_call_nonvirtual_long_method_a as *const () as usize;
    t[86] = jni_call_nonvirtual_float_method_v as *const () as usize;
    t[87] = jni_call_nonvirtual_float_method_a as *const () as usize;
    t[89] = jni_call_nonvirtual_double_method_v as *const () as usize;
    t[90] = jni_call_nonvirtual_double_method_a as *const () as usize;
    t[92] = jni_call_nonvirtual_void_method_v as *const () as usize;
    t[93] = jni_call_nonvirtual_void_method_a as *const () as usize;

    // CallStatic<Type>Method/V/A (groups of 3); the `...` slots are wired above.
    t[115] = jni_call_static_object_method_v as *const () as usize;
    t[116] = jni_call_static_object_method_a as *const () as usize;
    t[118] = jni_call_static_boolean_method_v as *const () as usize;
    t[119] = jni_call_static_boolean_method_a as *const () as usize;
    t[121] = jni_call_static_byte_method_v as *const () as usize;
    t[122] = jni_call_static_byte_method_a as *const () as usize;
    t[124] = jni_call_static_char_method_v as *const () as usize;
    t[125] = jni_call_static_char_method_a as *const () as usize;
    t[127] = jni_call_static_short_method_v as *const () as usize;
    t[128] = jni_call_static_short_method_a as *const () as usize;
    t[130] = jni_call_static_int_method_v as *const () as usize;
    t[131] = jni_call_static_int_method_a as *const () as usize;
    t[133] = jni_call_static_long_method_v as *const () as usize;
    t[134] = jni_call_static_long_method_a as *const () as usize;
    t[136] = jni_call_static_float_method_v as *const () as usize;
    t[137] = jni_call_static_float_method_a as *const () as usize;
    t[139] = jni_call_static_double_method_v as *const () as usize;
    t[140] = jni_call_static_double_method_a as *const () as usize;
    t[142] = jni_call_static_void_method_v as *const () as usize;
    t[143] = jni_call_static_void_method_a as *const () as usize;

    // Instance field access
    t[94] = jni_get_field_id as *const () as usize;
    t[95] = jni_get_object_field as *const () as usize;
    t[96] = jni_get_boolean_field as *const () as usize;
    t[97] = jni_get_byte_field as *const () as usize;
    t[98] = jni_get_char_field as *const () as usize;
    t[99] = jni_get_short_field as *const () as usize;
    t[100] = jni_get_int_field as *const () as usize;
    t[101] = jni_get_long_field as *const () as usize;
    t[102] = jni_get_float_field as *const () as usize;
    t[103] = jni_get_double_field as *const () as usize;
    t[104] = jni_set_object_field as *const () as usize;
    t[105] = jni_set_boolean_field as *const () as usize;
    t[106] = jni_set_byte_field as *const () as usize;
    t[107] = jni_set_char_field as *const () as usize;
    t[108] = jni_set_short_field as *const () as usize;
    t[109] = jni_set_int_field as *const () as usize;
    t[110] = jni_set_long_field as *const () as usize;
    t[111] = jni_set_float_field as *const () as usize;
    t[112] = jni_set_double_field as *const () as usize;

    // Static method IDs
    t[113] = jni_get_static_method_id as *const () as usize;

    // Static field access
    t[144] = jni_get_static_field_id as *const () as usize;
    t[145] = jni_get_static_object_field as *const () as usize;
    t[146] = jni_get_static_boolean_field as *const () as usize;
    t[147] = jni_get_static_byte_field as *const () as usize;
    t[148] = jni_get_static_char_field as *const () as usize;
    t[149] = jni_get_static_short_field as *const () as usize;
    t[150] = jni_get_static_int_field as *const () as usize;
    t[151] = jni_get_static_long_field as *const () as usize;
    t[152] = jni_get_static_float_field as *const () as usize;
    t[153] = jni_get_static_double_field as *const () as usize;
    t[154] = jni_set_static_object_field as *const () as usize;
    t[155] = jni_set_static_boolean_field as *const () as usize;
    t[156] = jni_set_static_byte_field as *const () as usize;
    t[157] = jni_set_static_char_field as *const () as usize;
    t[158] = jni_set_static_short_field as *const () as usize;
    t[159] = jni_set_static_int_field as *const () as usize;
    t[160] = jni_set_static_long_field as *const () as usize;
    t[161] = jni_set_static_float_field as *const () as usize;
    t[162] = jni_set_static_double_field as *const () as usize;

    // String operations
    t[163] = jni_new_string as *const () as usize;
    t[164] = jni_get_string_length as *const () as usize;
    t[165] = jni_get_string_chars as *const () as usize;
    t[166] = jni_release_string_chars as *const () as usize;
    t[167] = jni_new_string_utf as *const () as usize;
    t[168] = jni_get_string_utf_length as *const () as usize;
    t[169] = jni_get_string_utf_chars as *const () as usize;
    t[170] = jni_release_string_utf_chars as *const () as usize;

    // Array operations
    t[171] = jni_get_array_length as *const () as usize;
    t[172] = jni_new_object_array as *const () as usize;
    t[173] = jni_get_object_array_element as *const () as usize;
    t[174] = jni_set_object_array_element as *const () as usize;
    t[175] = jni_new_boolean_array as *const () as usize;
    t[176] = jni_new_byte_array as *const () as usize;
    t[177] = jni_new_char_array as *const () as usize;
    t[178] = jni_new_short_array as *const () as usize;
    t[179] = jni_new_int_array as *const () as usize;
    t[180] = jni_new_long_array as *const () as usize;
    t[181] = jni_new_float_array as *const () as usize;
    t[182] = jni_new_double_array as *const () as usize;
    t[183] = jni_get_boolean_array_elements as *const () as usize;
    t[184] = jni_get_byte_array_elements as *const () as usize;
    t[185] = jni_get_char_array_elements as *const () as usize;
    t[186] = jni_get_short_array_elements as *const () as usize;
    t[187] = jni_get_int_array_elements as *const () as usize;
    t[188] = jni_get_long_array_elements as *const () as usize;
    t[189] = jni_get_float_array_elements as *const () as usize;
    t[190] = jni_get_double_array_elements as *const () as usize;
    t[191] = jni_release_boolean_array_elements as *const () as usize;
    t[192] = jni_release_byte_array_elements as *const () as usize;
    t[193] = jni_release_char_array_elements as *const () as usize;
    t[194] = jni_release_short_array_elements as *const () as usize;
    t[195] = jni_release_int_array_elements as *const () as usize;
    t[196] = jni_release_long_array_elements as *const () as usize;
    t[197] = jni_release_float_array_elements as *const () as usize;
    t[198] = jni_release_double_array_elements as *const () as usize;
    t[199] = jni_get_boolean_array_region as *const () as usize;
    t[200] = jni_get_byte_array_region as *const () as usize;
    t[201] = jni_get_char_array_region as *const () as usize;
    t[202] = jni_get_short_array_region as *const () as usize;
    t[203] = jni_get_int_array_region as *const () as usize;
    t[204] = jni_get_long_array_region as *const () as usize;
    t[205] = jni_get_float_array_region as *const () as usize;
    t[206] = jni_get_double_array_region as *const () as usize;
    t[207] = jni_set_boolean_array_region as *const () as usize;
    t[208] = jni_set_byte_array_region as *const () as usize;
    t[209] = jni_set_char_array_region as *const () as usize;
    t[210] = jni_set_short_array_region as *const () as usize;
    t[211] = jni_set_int_array_region as *const () as usize;
    t[212] = jni_set_long_array_region as *const () as usize;
    t[213] = jni_set_float_array_region as *const () as usize;
    t[214] = jni_set_double_array_region as *const () as usize;

    // RegisterNatives / UnregisterNatives
    t[215] = jni_register_natives as *const () as usize;
    t[216] = jni_unregister_natives as *const () as usize;

    // Synchronization
    t[217] = jni_monitor_enter as *const () as usize;
    t[218] = jni_monitor_exit as *const () as usize;

    // JavaVM
    t[219] = jni_get_java_vm as *const () as usize;

    // String region operations
    t[220] = jni_get_string_region as *const () as usize;
    t[221] = jni_get_string_utf_region as *const () as usize;

    // Critical array / string operations
    t[222] = jni_get_primitive_array_critical as *const () as usize;
    t[223] = jni_release_primitive_array_critical as *const () as usize;
    t[224] = jni_get_string_critical as *const () as usize;
    t[225] = jni_release_string_critical as *const () as usize;

    // Weak global references
    t[226] = jni_new_weak_global_ref as *const () as usize;
    t[227] = jni_delete_weak_global_ref as *const () as usize;

    // Exception check
    t[228] = jni_exception_check as *const () as usize;

    // NIO Direct ByteBuffer (JNI 1.4)
    t[229] = jni_new_direct_byte_buffer as *const () as usize;
    t[230] = jni_get_direct_buffer_address as *const () as usize;
    t[231] = jni_get_direct_buffer_capacity as *const () as usize;

    // GetObjectRefType (JNI 1.6)
    t[232] = jni_get_object_ref_type as *const () as usize;

    // GetModule (JNI 9+)
    t[233] = jni_get_module as *const () as usize;

    // IsVirtualThread (JNI 21+) and GetStringUTFLengthAsLong (JNI 24+). Both
    // are declared by JDK 25's `jni.h`, so a native compiled against it can
    // load either slot; before the table was widened those two loads read off
    // the end of the array.
    t[234] = jni_is_virtual_thread as *const () as usize;
    t[235] = jni_get_string_utf_length_as_long as *const () as usize;

    Box::new(t)
}

// ---------------------------------------------------------------------------
// JavaVM Invocation Interface
// ---------------------------------------------------------------------------

/// `jint DestroyJavaVM(JavaVM *vm)` — invocation-table slot 3.
///
/// HotSpot semantics: the calling thread waits until it is the only remaining
/// non-daemon thread, then unloads the VM; after a successful return the VM may
/// not be used and (per the spec) cannot be recreated in the same process. We
/// honour the load-bearing part of that contract for an embedded VM: tear the VM
/// down by invoking the embedder-registered teardown hook (which drops the parked
/// `Vm`, releasing its `Arc<SharedVm>` / heap / threads, clears the calling
/// thread's JNI TLS context, and resets the one-VM-per-process registry so
/// `JNI_GetCreatedJavaVMs` then reports 0).
///
/// We do NOT block waiting for other non-daemon threads: the embedding contract
/// (see `docs/feature-designs/embedding-api.md`) is that the host calls
/// `DestroyJavaVM` from the creating thread once it has quiesced its own Java
/// activity — mirroring how an embedder drives a single VM. Returns the hook's
/// status (`JNI_OK` when no hook is registered, i.e. nothing to tear down).
extern "C" fn jni_destroy_java_vm(_vm: JavaVM) -> JInt {
    run_destroy_vm_hook()
}

/// `jint GetEnv(JavaVM *vm, void **env, jint version)` — invocation-table
/// slot 6.
///
/// Interpreter round i1 wave 11, lane L1: a version naming another interface
/// than JNI — JVMTI (`0x30000000 | ...`, e.g. `JVMTI_VERSION_1_2`), or the
/// retired JVMPI / JVMDI — answers `JNI_EVERSION` with `*env` set to NULL,
/// as HotSpot does for an interface it does not provide. It answered the
/// `JNIEnv*`, so a library probing for JVMTI from `JNI_OnLoad` (async-profiler
/// does) got a JNI table and called through it as a `jvmtiEnv` — slot numbers
/// of one table taken as functions of the other. Wave 12: a supported JVMTI
/// version answers a C `jvmtiEnv` (`jvmti::native_env`) in
/// `experimental-debug` builds; JVMPI, JVMDI and other versions still answer
/// `JNI_EVERSION`.
extern "C" fn jni_get_env(_vm: JavaVM, env: *mut *mut std::ffi::c_void, version: JInt) -> JInt {
    if env.is_null() {
        return JNI_ERR;
    }
    if version & JNI_VERSION_INTERFACE_MASK != 0 {
        // Wave 12: a JVMTI version this VM provides gets a C `jvmtiEnv`
        // (`jvmti::native_env`, debug builds, where JVMTI lives).
        #[cfg(feature = "experimental-debug")]
        {
            if let Some(jvmti) = crate::jvmti::native_env::new_env_for_version(version) {
                // SAFETY: `env` is non-null (checked above) and points at
                // the caller's `void*` out-parameter.
                unsafe {
                    *env = jvmti;
                }
                return JNI_OK;
            }
        }
        // SAFETY: `env` is non-null (checked above) and points at the
        // caller's `void*` out-parameter.
        unsafe {
            *env = std::ptr::null_mut();
        }
        return JNI_EVERSION;
    }
    // Interpreter round i1 wave 14: a JNI version the VM does not provide is
    // `JNI_EVERSION` with a NULL env, as in HotSpot
    // (`Threads::is_supported_jni_version_including_1_1`); every value
    // answered the `JNIEnv*`.
    if !is_supported_jni_version(version) {
        // SAFETY: `env` is non-null (checked above) and points at the
        // caller's `void*` out-parameter.
        unsafe {
            *env = std::ptr::null_mut();
        }
        return JNI_EVERSION;
    }
    let jni_env = get_jni_env();
    // SAFETY: `env` is non-null (checked above) and points at the caller's
    // `void*` out-parameter.
    unsafe {
        *env = jni_env as *mut std::ffi::c_void;
    }
    JNI_OK
}

/// The JNI versions JDK 25's HotSpot accepts from `GetEnv`: 1.1, 1.2, 1.4,
/// 1.6, 1.8, 9, 10, 19, 20, 21 and 24 (`jni.h`'s `JNI_VERSION_*`).
fn is_supported_jni_version(version: JInt) -> bool {
    matches!(
        version,
        0x0001_0001
            | 0x0001_0002
            | 0x0001_0004
            | 0x0001_0006
            | 0x0001_0008
            | 0x0009_0000
            | 0x000a_0000
            | 0x0013_0000
            | 0x0014_0000
            | 0x0015_0000
            | 0x0018_0000
    )
}

/// Write the process-global `JNIEnv*` into `*penv`, if non-null.
///
/// `JNIEnv` is a pointer-to-pointer-to-function-table (`*const *const usize`):
/// native code dereferences it twice — `(*env)[slot]`. We therefore hand back
/// [`get_jni_env`] (`&JNI_TABLE_PTR`), NOT the table-array pointer
/// `JNI_TABLE_PTR.load()` itself, which is one indirection too shallow and would
/// make `(*env)[slot]` read a function's code bytes as a slot pointer (the
/// historical `AttachCurrentThread` stub had this bug, harmless only because no
/// caller ever drove the table through the attach env).
fn write_jni_env(penv: *mut *mut std::ffi::c_void) {
    if penv.is_null() {
        return;
    }
    let env = get_jni_env();
    unsafe {
        *penv = env as *mut std::ffi::c_void;
    }
}

extern "C" fn jni_attach_current_thread(
    _vm: JavaVM,
    penv: *mut *mut std::ffi::c_void,
    args: *mut std::ffi::c_void,
) -> JInt {
    attach_current_thread_impl(penv, args, false)
}

/// `AttachCurrentThreadAsDaemon` (invocation slot 7) — identical to
/// `AttachCurrentThread` except the registered thread is a **daemon** (the VM's
/// non-daemon-wait at shutdown ignores it). Wired in Step 7.
extern "C" fn jni_attach_current_thread_as_daemon(
    _vm: JavaVM,
    penv: *mut *mut std::ffi::c_void,
    args: *mut std::ffi::c_void,
) -> JInt {
    attach_current_thread_impl(penv, args, true)
}

/// Shared body for `AttachCurrentThread` / `AttachCurrentThreadAsDaemon`.
///
/// When the foreign-attach gate is off (default until Step 7) this preserves the
/// historical env-only behaviour. When on, a genuinely foreign thread is
/// registered as a first-class GC-safe Java thread (see
/// `foreign-thread-attach.md`).
fn attach_current_thread_impl(
    penv: *mut *mut std::ffi::c_void,
    args: *mut std::ffi::c_void,
    daemon: bool,
) -> JInt {
    // Already-attached fast path. A thread whose JNI context is set — the
    // bootstrap/creating thread (main, id 0), or a foreign thread re-attaching —
    // is a no-op that just hands back the env pointer (matches HotSpot: a
    // redundant AttachCurrentThread returns JNI_OK).
    let has_context = JNI_SHARED_VM.with(|c| c.borrow().is_some());
    if has_context {
        write_jni_env(penv);
        tracing::trace!("JNI AttachCurrentThread: thread already attached");
        return JNI_OK;
    }

    if foreign_attach_enabled() {
        // Repair case: this OS thread still OWNS a foreign JvmThread but its JNI
        // context was cleared (a nested true-JNI-native call's guard clears the
        // TLS context on return). Re-publish rather than attaching a second time
        // (which would leak a JvmThread and inflate alive_count).
        if is_foreign_attached() {
            // The attachment's own VM (gc-common w10-c), not the newest one.
            if let Some(shared) = foreign_attachment_vm() {
                set_jni_context_arc(shared);
            }
            with_foreign_thread(|jt| set_jni_thread(jt as *mut JvmThread));
            write_jni_env(penv);
            return JNI_OK;
        }
        // Genuine first attach: resolve the live VM from "the JavaVM*".
        let shared = match process_vm() {
            Some(s) => s,
            None => return JNI_ERR,
        };
        // SAFETY: `args`, when non-null, is a valid `JavaVMAttachArgs` per ABI.
        let name = unsafe { read_attach_name(args) };
        // Register, go idle (§3.3), publish the TLS context last.
        let ledger = Arc::clone(shared.mem.gc_barrier.pause_ledger());
        attach_foreign_thread_idle(shared, daemon, name.as_deref());
        count_attachment_raw_locals(ledger);
        write_jni_env(penv);
        tracing::debug!("JNI AttachCurrentThread: foreign thread attached + registered");
        return JNI_OK;
    }

    // Gate off — historical behaviour: env pointer only, no registration.
    write_jni_env(penv);
    tracing::debug!("JNI AttachCurrentThread: thread attached (env-only, gate off)");
    JNI_OK
}

/// Is a JNI native method of the thread this OS thread's JNI binding names on
/// its stack? `JniContextGuard::install_for_native` sets the thread's
/// `jni_native_class` for the dispatch and restores it at the return, so it
/// is `Some` from the outermost native's entry to its exit. `try_with`: the
/// detach can arrive from a TSD destructor with the TLS gone (then: no).
fn jni_native_frame_on_stack() -> bool {
    jni_native_dispatch_thread().is_some()
}

/// gcd d10/j: the `JvmThread` of the JNI native dispatch in progress on this
/// OS thread (`JniContextGuard::install_for_native` set its
/// `jni_native_class`), if any: a thread with live Java frames under a native.
fn jni_native_dispatch_thread() -> Option<*mut JvmThread> {
    let bound = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    // SAFETY: a non-null `JNI_THREAD` points at a live `JvmThread` while it
    // is installed (`set_jni_thread`'s contract); one field is read.
    (!bound.is_null() && unsafe { (*(bound as *const JvmThread)).jni_native_class.is_some() })
        .then_some(bound as *mut JvmThread)
}

/// gcd d4/k2 (lane m's request 1, `gc_quiescence::note_raw_jni_locals_open`):
/// count this OS thread's just-made foreign attachment in its VM's pause
/// ledger as holding RAW JNI locals, for the life of the attachment, when its
/// locals are raw (`local_refs_may_be_held_raw`: unless both
/// `CRATONVM_JNI_INDIRECT_LOCALS` and `CRATONVM_JNI_FOREIGN_TRANSITIONS` are on,
/// an attached thread's handles are addresses its host code may keep across
/// idle windows) and anything reads the count ([`raw_locals_counted`]).
/// Remembered in the attachment's `ForeignThreadBox`, which closes it at the
/// detach or when the OS thread exits attached.
///
/// Only `AttachCurrentThread` counts: the VM's own attached service threads
/// (the AIO dispatcher, `run_attached_service`) are Rust and hold no handle
/// outside a Java call; a JNI native they reach through Java is counted by its
/// dispatch like any other.
fn count_attachment_raw_locals(ledger: Arc<cratonvm_gc::gc_quiescence::PauseLedger>) {
    if !raw_locals_counted() || !local_refs_may_be_held_raw() {
        return;
    }
    let _ = FOREIGN_THREAD_BOX.try_with(|c| {
        if let Ok(mut slot) = c.try_borrow_mut() {
            if let Some(bx) = slot.as_mut() {
                if bx.raw_locals_ledger.is_none() {
                    cratonvm_gc::gc_quiescence::note_raw_jni_locals_open(&ledger);
                    bx.raw_locals_ledger = Some(ledger);
                }
            }
        }
    });
}

extern "C" fn jni_detach_current_thread(_vm: JavaVM) -> JInt {
    // Reachable from a `pthread` TSD destructor with this thread's TLS already
    // destroyed — see the note above `is_foreign_attached`. Nothing below may
    // use `LocalKey::with`.
    //
    // `is_foreign_attached()` answers `false` in that state, and the
    // never-attached tail below is `try_with`-safe too, so the whole function
    // degrades to "nothing to detach, report success" rather than panicking
    // out of an `extern "C"` frame.
    if is_foreign_attached() {
        // JNI forbids detaching a thread that still has Java frames on its stack;
        // for us that means a call is in flight on this OS thread (depth > 0).
        // Return JNI_ERR rather than corrupt state (matches HotSpot).
        if FOREIGN_CALL_DEPTH.try_with(|c| c.get()).unwrap_or(0) != 0 {
            tracing::warn!("JNI DetachCurrentThread: refusing detach with a Java call in flight");
            return JNI_ERR;
        }
        // The attachment's own VM (gc-common w10-c): `process_vm()` is the
        // newest VM, whose barrier this thread was never blocked in.
        if let Some(shared) = foreign_attachment_vm() {
            detach_foreign_thread_idle(&shared);
        } else {
            // No live VM (process shutdown) — just drop our owned box.
            let _ = FOREIGN_THREAD_BOX.try_with(|c| *c.borrow_mut() = None);
            let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
            if let Some(base) = FOREIGN_ATTACH_FRAME.try_with(Cell::take).ok().flatten() {
                truncate_local_frames(base);
            }
            let _ = FOREIGN_ATTACH_VM.try_with(|c| c.try_borrow_mut().map(|mut w| w.take()));
        }
        clear_jni_thread();
        clear_jni_context();
        tracing::debug!("JNI DetachCurrentThread: foreign thread detached");
        return JNI_OK;
    }

    // gcd d3/k: a VM thread calling from inside a JNI native method has Java
    // frames below the native. HotSpot refuses (`JNI_ERR`) and detaches
    // nothing; this cleared the JNI context instead, so every JNIEnv call the
    // native made after it was answered NULL / 0 (`with_jni_context` found no
    // thread), and the dispatch's `JniContextGuard` put it back only at the
    // native's return.
    if jni_native_frame_on_stack() {
        tracing::warn!("JNI DetachCurrentThread: refusing detach from inside a native method");
        return JNI_ERR;
    }

    // Non-foreign thread (bootstrap/creating thread, or never-attached): clear
    // the JNI TLS context if present — historical behaviour, unchanged.
    let had_context = JNI_SHARED_VM
        .try_with(|c| c.borrow().is_some())
        .unwrap_or(false);
    if had_context {
        clear_jni_context();
        clear_jni_thread();
        tracing::debug!("JNI DetachCurrentThread: thread detached");
    } else {
        tracing::trace!("JNI DetachCurrentThread: thread was not attached");
    }
    JNI_OK
}

fn build_invoke_table() -> Box<[usize; JNI_INVOKE_FUNCTION_COUNT]> {
    let stub = jni_stub as *const () as usize;
    let mut t = [stub; JNI_INVOKE_FUNCTION_COUNT];
    t[3] = jni_destroy_java_vm as *const () as usize;
    t[4] = jni_attach_current_thread as *const () as usize;
    t[5] = jni_detach_current_thread as *const () as usize;
    t[6] = jni_get_env as *const () as usize;
    t[7] = jni_attach_current_thread_as_daemon as *const () as usize; // AttachCurrentThreadAsDaemon
    Box::new(t)
}

// ---------------------------------------------------------------------------
// Global table storage
// ---------------------------------------------------------------------------

// The two JNI function tables are leaked, process-lifetime singletons. They are
// stored in `AtomicPtr`s rather than `static mut` so that reads/writes are
// well-defined under concurrent access (raw `static mut` access is UB-adjacent
// on the 2024 edition). `JNI_TABLE_INIT` (a `Once`) still guarantees the values
// are written exactly once; the atomics only make the publication well-defined.
static JNI_TABLE_PTR: std::sync::atomic::AtomicPtr<usize> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static JNI_INVOKE_TABLE_PTR: std::sync::atomic::AtomicPtr<usize> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static JNI_TABLE_INIT: std::sync::Once = std::sync::Once::new();

fn init_jni_table() {
    use std::sync::atomic::Ordering;
    JNI_TABLE_INIT.call_once(|| {
        let table = build_function_table();
        let raw = Box::into_raw(table) as *mut usize; // OWNERSHIP: transferred to JNI_TABLE_PTR static; intentionally leaked (process-lifetime singleton)
        let invoke_table = build_invoke_table();
        let invoke_raw = Box::into_raw(invoke_table) as *mut usize; // OWNERSHIP: transferred to JNI_INVOKE_TABLE_PTR static; intentionally leaked (process-lifetime singleton)
        JNI_TABLE_PTR.store(raw, Ordering::Release);
        JNI_INVOKE_TABLE_PTR.store(invoke_raw, Ordering::Release);
    });
}

/// Get a JNIEnv pointer.
/// JNIEnv = `*const *const usize` — a pointer to a pointer to the function table.
pub fn get_jni_env() -> JNIEnv {
    init_jni_table();
    // `AtomicPtr<usize>` has the same layout as `*mut usize`, so the address of
    // the atomic itself is a valid `*const *const usize` pointing at the table
    // pointer published by `init_jni_table`.
    std::ptr::addr_of!(JNI_TABLE_PTR).cast()
}

/// Get a JavaVM pointer.
pub fn get_java_vm() -> JavaVM {
    init_jni_table();
    std::ptr::addr_of!(JNI_INVOKE_TABLE_PTR).cast()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::Arc;

    /// True when the process-global `PROCESS_VM` cell still holds *this* weak
    /// handle.
    ///
    /// `PROCESS_VM` is one cell for the whole process, and `Vm::new`
    /// (`vm/src/vm/vm_init.rs`) republishes it unconditionally for every VM it
    /// builds — deliberately, so a host thread calling `AttachCurrentThread`
    /// can always resolve *a* live VM. Production code has no access to
    /// [`PROCESS_VM_TEST_LOCK`], and this crate's own tests construct a `Vm` at
    /// 60+ call sites, so a test holding that lock can still have the cell
    /// overwritten underneath it by a concurrently running one. Assertions
    /// about what the cell contains are therefore only meaningful while it
    /// still refers to the VM this test published.
    ///
    /// `Weak::ptr_eq` compares allocations, and a live `Weak` keeps its
    /// allocation from being reused, so this stays exact even after the strong
    /// count reaches zero — unlike comparing raw addresses, which the allocator
    /// is free to hand back out for the next same-sized `SharedVm`.
    fn process_vm_cell_holds(weak: &Weak<SharedVm>) -> bool {
        PROCESS_VM
            .lock()
            .as_ref()
            .is_some_and(|published| Weak::ptr_eq(published, weak))
    }

    /// Serializes tests that mutate the process-global `PROCESS_VM` cell or the
    /// foreign-attach TLS so they don't race each other under the parallel test
    /// runner (the cell and the JNI invocation table are process-wide).
    static PROCESS_VM_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// The JNI thread-locals a test installs, cleared when the guard drops --
    /// on a failing assertion's unwind too (gc-common w9-g; wave-8 lesson (c)).
    ///
    /// A test used to install them with `set_jni_context_arc` /
    /// `set_jni_thread` and clear them on its last line, which a panicking
    /// assertion never reaches. The unwind then dropped the test's
    /// `JvmThread` while `JNI_THREAD` still pointed at it, and left the
    /// `Arc<SharedVm>` in `JNI_SHARED_VM` for the thread's TLS teardown to
    /// drop; C8's G1 test deadlocked the whole `--lib` run that way in the
    /// wave-8 verification. Bind the guard AFTER the `SharedVm` and the
    /// `JvmThread` it points at, so it drops before them. It also closes any
    /// local frame the test left open, and drops a pending JNI exception.
    #[must_use = "the JNI thread-locals are cleared when the guard drops"]
    struct JniTls {
        frames_base: usize,
    }

    impl JniTls {
        /// `set_jni_context_arc(shared)`.
        fn context(shared: &Arc<SharedVm>) -> Self {
            let frames_base = local_frame_depth();
            set_jni_context_arc(Arc::clone(shared));
            JniTls { frames_base }
        }

        /// `replace_jni_context(shared)`, for a test holding a `Vm` rather than
        /// an `Arc<SharedVm>`. What was installed before is discarded, as the
        /// `let _prev = replace_jni_context(..)` it replaces did.
        fn replace(shared: &SharedVm) -> Self {
            let frames_base = local_frame_depth();
            drop(replace_jni_context(shared));
            JniTls { frames_base }
        }

        /// Installs nothing: for a test of the set / clear API itself, so a
        /// failing assertion between its own set and clear still clears.
        fn cleanup_only() -> Self {
            JniTls {
                frames_base: local_frame_depth(),
            }
        }

        /// Also `set_jni_thread(thread)`; cleared with the context.
        fn with_thread(self, thread: *mut JvmThread) -> Self {
            set_jni_thread(thread);
            self
        }
    }

    impl Drop for JniTls {
        fn drop(&mut self) {
            truncate_local_frames(self.frames_base);
            let _ = take_jni_pending_exception();
            clear_jni_thread();
            clear_jni_context();
        }
    }

    /// The guard clears what it installed on a panic's unwind, not only at the
    /// end of a passing test.
    #[test]
    fn jni_tls_guard_clears_on_unwind() {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "jni-tls-guard");
        let base = local_frame_depth();
        let thread_ptr: *mut JvmThread = &mut thread;
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _jni = JniTls::context(&shared).with_thread(thread_ptr);
            push_local_frame(4);
            assert!(with_jni_context(|_, _| ()).is_some(), "installed");
            panic!("a failing assertion inside a JNI test");
        }));
        assert!(unwound.is_err());
        assert!(with_shared_vm(|_| ()).is_none(), "context cleared by the unwind");
        assert!(JNI_THREAD.with(Cell::get).is_null(), "thread cleared by the unwind");
        assert_eq!(local_frame_depth(), base, "the test's open frame was closed");
        let _ = thread;
    }

    #[test]
    fn global_refs_add_remove() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        // Handle must be non-zero and have the global-ref tag bit set.
        assert_ne!(handle, 0);
        assert_eq!(handle & 1, 1, "global ref handle must have bit 0 set");
        // Resolving the handle must yield the original object.
        assert_eq!(refs.resolve(handle), Some(obj));
        assert_eq!(refs.count(), 1);
        // Removal succeeds and the handle no longer resolves.
        assert!(refs.remove(handle));
        assert_eq!(refs.count(), 0);
    }

    /// i1 wave 5, lane L4: the three JNI `jlong` producers that did not mint
    /// (call arguments, `SetStaticLongField`, the long-array write-backs) now
    /// share `mint_if_smuggled_jlong`. An object's address is registered; an
    /// ordinary number and a double's bits are not.
    #[test]
    fn jni_jlong_producers_mint_object_addresses_only() {
        use crate::config::VmConfig;
        use crate::memory::smuggled_longs::is_minted;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let heap = &shared.mem.heap;
        let a = heap.alloc_object(crate::classloading::ClassId::new(0), 1);
        let b = heap.alloc_object(crate::classloading::ClassId::new(0), 1);
        let c = heap.alloc_object(crate::classloading::ClassId::new(0), 1);
        let a_bits = a.as_ptr() as u64;
        let b_bits = b.as_ptr() as u64;
        let c_bits = c.as_ptr() as u64;
        assert!(!is_minted(heap, a_bits) && !is_minted(heap, b_bits));

        // Call-argument path (`Call*MethodA`, and `*MethodV` via
        // `va_list_to_jvalues`).
        let args = [JValue { j: a_bits as i64 }, JValue { j: 4096 }];
        let values = unsafe { jvalues_to_values(heap, args.as_ptr(), b"JJ") };
        assert_eq!(values, vec![Value::Long(a_bits as i64), Value::Long(4096)]);
        assert!(
            is_minted(heap, a_bits),
            "a jobject passed as a jlong argument"
        );
        assert!(!is_minted(heap, 4096), "an ordinary jlong argument");

        // Element write-back path (`Release<Long>ArrayElements`,
        // `ReleasePrimitiveArrayCritical`): only a `Long` element mints.
        mint_if_smuggled_long_value(heap, &Value::Double(f64::from_bits(c_bits)));
        assert!(!is_minted(heap, c_bits), "a double element never mints");
        mint_if_smuggled_long_value(heap, &Value::Long(b_bits as i64));
        assert!(is_minted(heap, b_bits), "a jobject written into a long[]");
    }

    #[test]
    fn local_frame_lifecycle() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut frame = JniLocalFrame::new();
        let jobj = frame.add(obj);
        assert_ne!(jobj, 0);
        assert_eq!(frame.refs.len(), 1);
        frame.remove(jobj);
        assert_eq!(frame.refs.len(), 0);
    }

    #[test]
    fn jobject_null_roundtrip() {
        assert!(jobject_to_obj(0).is_none());
    }

    /// gc-common w5-g (`handoff-w4c-infallible-callers-residue` §2): a JNI
    /// allocation the heap refuses answers `NULL` with a pending exception --
    /// JNI's `OutOfMemoryError` contract -- instead of reaching an infallible
    /// allocator that aborts the process on G1. A granted one raises nothing.
    #[test]
    fn a_refused_jni_allocation_is_null_with_a_pending_exception() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _ = take_jni_pending_exception();

        assert!(jni_alloc_or_oom(&shared, "a test object", |_| None).is_none());
        assert!(
            take_jni_pending_exception().is_some(),
            "a refused allocation must leave an exception pending"
        );

        let granted = jni_alloc_or_oom(&shared, "a test object", |heap| {
            heap.try_alloc_object_full(ClassId::new(0), 1)
        });
        assert!(granted.is_some());
        assert!(take_jni_pending_exception().is_none());
    }

    #[test]
    fn process_vm_publish_and_resolve() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        // Build a VM Arc and publish it as the process-global cell.
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let our_weak = Arc::downgrade(&shared);
        set_process_vm(&shared);

        // The attach path can now resolve the live VM from "the JavaVM*" —
        // provided a concurrently running test has not republished the cell in
        // the meantime (see `process_vm_cell_holds`). In a serial run, and in
        // the overwhelming majority of parallel ones, it has not.
        if process_vm_cell_holds(&our_weak) {
            let resolved = process_vm().expect("process_vm should resolve after publish");
            assert!(
                Arc::ptr_eq(&shared, &resolved),
                "process_vm must return the same SharedVm that was published"
            );
            drop(resolved);
        }

        drop(shared);

        // The property that actually matters, and the one that is fully
        // deterministic: dropping every owning `Arc` really does destroy the
        // VM. Nothing can hold a strong reference here — `set_process_vm`
        // stores a `Weak`, and `SharedVm::new` runs *before* the `Arc` exists,
        // so the threads it spawns have nothing to clone.
        assert!(
            our_weak.upgrade().is_none(),
            "dropping every owning Arc must destroy the SharedVm; a surviving \
             strong reference would let process_vm() resurrect a dropped VM"
        );

        // …and the cell must not resurrect it. This assertion is conditioned
        // for the same reason as the one above: once our weak is dead it can
        // never upgrade, so a `Some` from `process_vm()` is provably some OTHER
        // test's live VM rather than ours — checking for `None` unconditionally
        // was the flake (assertion at this line, "process_vm must return None
        // once the VM has been dropped": 2 of 25 full debug-suite runs, and 1
        // of 25 release runs with CRATONVM_LOCK_ORDER_CHECK=1).
        if process_vm_cell_holds(&our_weak) {
            assert!(
                process_vm().is_none(),
                "process_vm must return None once the VM has been dropped"
            );
        }
    }

    /// With two live VMs `process_vm_strict` must refuse, but the flag address
    /// a JIT poll tested still names exactly one of them.
    #[test]
    fn vm_for_stw_flag_addr_names_the_vm_whose_flag_it_is() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let a = Arc::new(SharedVm::new(VmConfig::default()));
        let b = Arc::new(SharedVm::new(VmConfig::default()));
        set_process_vm(&a);
        set_process_vm(&b);
        let a_flag = a.mem.gc_barrier.stw_requested_flag_addr() as usize;
        let b_flag = b.mem.gc_barrier.stw_requested_flag_addr() as usize;
        assert_ne!(a_flag, b_flag, "each VM owns its own stop-the-world flag");
        let got_b = vm_for_stw_flag_addr(b_flag).expect("b's flag names b");
        assert!(Arc::ptr_eq(&got_b, &b));
        let got_a = vm_for_stw_flag_addr(a_flag).expect("a's flag names a");
        assert!(Arc::ptr_eq(&got_a, &a));
        assert!(vm_for_stw_flag_addr(0).is_none());
    }

    /// `DestroyJavaVM`'s teardown hook fires once and is one-shot: a registered
    /// hook runs on the first `DestroyJavaVM`, and a second call (the VM now
    /// gone) is a no-op `JNI_OK` without re-running it — mirroring HotSpot, where
    /// the VM cannot be destroyed twice.
    #[test]
    fn destroy_vm_hook_runs_once_then_clears() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn hook() -> JInt {
            CALLS.fetch_add(1, Ordering::SeqCst);
            JNI_OK
        }
        CALLS.store(0, Ordering::SeqCst);
        // With no hook registered, teardown is a no-op success.
        // (Take any stale hook a prior test/run left behind first.)
        let _ = run_destroy_vm_hook();
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);

        set_destroy_vm_hook(hook);
        assert_eq!(run_destroy_vm_hook(), JNI_OK);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "hook must run exactly once"
        );
        // Second DestroyJavaVM: hook was taken, so no re-run, still JNI_OK.
        assert_eq!(run_destroy_vm_hook(), JNI_OK);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "hook must not run again after being taken"
        );
    }

    /// The detach path must ANSWER, not panic, when it is reached from a
    /// `pthread` thread-specific-data destructor.
    ///
    /// That is how `DetachCurrentThread` actually arrives for a host thread a
    /// JNI library attached: the library registers its per-thread cleanup with
    /// `pthread_key_create`, and glibc runs those destructors in
    /// `__nptl_deallocate_tsd`, which is AFTER `__call_tls_dtors` — so every
    /// Rust `thread_local!` on the thread is already destroyed and
    /// `LocalKey::with` panics. Panicking out of an `extern "C"` frame is
    /// undefined behaviour, and what it did in practice was abort the process:
    /// 183 of 206 Hibernate Reactive classes on rc=134, each one *after* it had
    /// printed a passing `@@RESULT`. See
    /// `hibernate-reactive-double-panic-abort-FIXED-20260901`.
    ///
    /// A regression re-panics inside a TLS destructor, which aborts the test
    /// process — so this fails loudly rather than quietly.
    #[cfg(unix)]
    #[test]
    fn detach_path_answers_from_a_pthread_tsd_destructor() {
        use std::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};

        static ANSWER: AtomicI32 = AtomicI32::new(-1);

        unsafe extern "C" fn tsd_dtor(_v: *mut std::ffi::c_void) {
            // Reached with this thread's Rust TLS already gone.
            ANSWER.store(i32::from(is_foreign_attached()), AtomicOrdering::SeqCst);
        }

        let mut key: libc::pthread_key_t = 0;
        assert_eq!(
            unsafe { libc::pthread_key_create(&mut key, Some(tsd_dtor)) },
            0,
            "pthread_key_create failed"
        );

        std::thread::Builder::new()
            .name("tsdprobe".to_string())
            .spawn(move || {
                // Give the thread some Rust TLS to tear down, then arm the
                // pthread key so its destructor runs after that teardown.
                let _ = is_foreign_attached();
                unsafe {
                    libc::pthread_setspecific(key, 1_usize as *const std::ffi::c_void);
                }
            })
            .unwrap()
            .join()
            .unwrap();

        assert_eq!(
            ANSWER.load(AtomicOrdering::SeqCst),
            0,
            "the TSD destructor never ran, so this test proved nothing"
        );
    }

    #[test]
    fn foreign_attach_factory_registers_shares_and_detaches() {
        use crate::classloading::ClassId;
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        // A bare SharedVm has no registered threads (Vm::new registers main).
        let baseline = shared.threads.thread_registry.alive_count();

        let raw = attach_foreign_thread(&shared, false, None);
        assert!(!raw.is_null());
        assert!(is_foreign_attached());
        assert_eq!(
            shared.threads.thread_registry.alive_count(),
            baseline + 1,
            "attach must add exactly one alive thread"
        );

        // Safety: `raw` points at the live, boxed foreign JvmThread parked in
        // this thread's FOREIGN_THREAD_BOX; the accessed fields are Arc/atomic
        // (interior mutability), so a shared reference is sufficient.
        let jt: &JvmThread = unsafe { &*raw };

        // The registry must SHARE this thread's root_snapshot Arc: an object
        // pushed into the JvmThread's snapshot is visible to the cross-thread
        // collector (`collect_all_root_snapshots`), which is how a GC initiator
        // on another thread scans this foreign thread's roots.
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        jt.root_snapshot.lock().push(obj);
        let collected = shared.threads.thread_registry.collect_all_root_snapshots();
        assert!(
            collected.contains(&obj),
            "registry must observe the foreign thread's root via the shared snapshot Arc"
        );

        // The registry must SHARE this thread's gc_block_state Arc too: setting
        // in_blocked_region on the JvmThread is visible to the initiator's
        // blocked-region maintenance (`dump_blocked_states`).
        let tid = jt.thread_id;
        jt.gc_block_state
            .in_blocked_region
            .store(true, std::sync::atomic::Ordering::Release);
        let blocked = shared.threads.thread_registry.dump_blocked_states();
        assert!(
            blocked.iter().any(|(t, blk, _)| *t == tid.0 && *blk),
            "registry must observe the foreign thread's blocked state via the shared Arc"
        );

        // Detach reclaims the thread and restores the baseline.
        assert!(detach_foreign_thread(&shared));
        assert!(!is_foreign_attached());
        assert_eq!(
            shared.threads.thread_registry.alive_count(),
            baseline,
            "detach must restore alive_count to baseline"
        );
        // Double-detach on a thread with no attachment is a clean no-op.
        assert!(!detach_foreign_thread(&shared));
    }

    /// gcd d2/i: the idle attach registers its thread STARTING, so a pause
    /// whose census runs before the thread is flagged blocked does not count
    /// it (and then wait forever for an idle host thread that never arrives).
    #[test]
    fn a_starting_foreign_attachment_is_in_no_pause_quota() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let counted_before = shared.threads.thread_registry.alive_count_and_os_tids().0;
        let alive_before = shared.threads.thread_registry.alive_count();
        let _raw = attach_foreign_thread_registered(&shared, false, None, false);
        assert_eq!(
            shared.threads.thread_registry.alive_count(),
            alive_before + 1,
            "registered and alive"
        );
        assert_eq!(
            shared.threads.thread_registry.alive_count_and_os_tids().0,
            counted_before,
            "but counted by no census until it is blocked and marked ready"
        );
        let tid = with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, std::sync::atomic::Ordering::Release);
            jt.thread_id
        })
        .unwrap();
        shared.threads.thread_registry.mark_stw_ready(tid);
        let (alive, blocked, _, blocked_tids) =
            shared.threads.thread_registry.alive_count_blocked_and_os_tids();
        assert_eq!(alive, counted_before + 1, "counted once ready");
        assert!(blocked >= 1 && blocked_tids.contains(&tid.0), "and excluded as blocked");
        shared.threads.thread_registry.mark_dead(tid);
        assert!(detach_foreign_thread(&shared));
    }

    /// The core GC-safety property (§3.3): an attached-but-idle foreign thread is
    /// modelled as GC-blocked, so a stop-the-world initiated by another thread
    /// does NOT wait for it — no deadlock, even though it is alive and counted in
    /// `alive_count`.
    #[test]
    fn foreign_idle_thread_excluded_from_stw() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::collections::HashMap;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        // A "main" initiator thread, registered alive.
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);

        // Attach a foreign thread and put it in the idle blocked region exactly
        // as `attach_current_thread_impl` does.
        let _raw = attach_foreign_thread(&shared, false, None);
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, std::sync::atomic::Ordering::Release)
        });
        let _ = shared.mem.gc_barrier.mark_blocked_region_enter();

        // Two alive threads (main + foreign), but the idle foreign thread is
        // excluded from `expected`, so the initiator waits for nobody.
        let alive = shared.threads.thread_registry.alive_count() as u32;
        assert_eq!(alive, 2);
        assert!(shared.mem.gc_barrier.request_stw(main_tid, alive));
        assert_eq!(
            shared.mem.gc_barrier.pending_count(),
            0,
            "idle foreign thread must be excluded from the STW expected-set"
        );
        shared.mem.gc_barrier.wait_for_all(); // returns immediately — no deadlock
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());

        // Teardown mirrors detach: mark dead, leave the region, reclaim.
        let tid = with_foreign_thread(|jt| jt.thread_id).unwrap();
        shared.threads.thread_registry.mark_dead(tid);
        shared.mem.gc_barrier.mark_blocked_region_leave();
        assert!(detach_foreign_thread(&shared));
    }

    /// `ForeignCallGuard` drives the idle↔running transition: the outermost call
    /// leaves the blocked region (becomes a counted mutator), nested calls only
    /// bump the depth counter, and the outermost return re-enters the region.
    #[test]
    fn foreign_call_guard_transitions() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::atomic::Ordering;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let our_weak = Arc::downgrade(&shared);
        set_process_vm(&shared);

        let _raw = attach_foreign_thread(&shared, false, None);
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release)
        });
        let _ = shared.mem.gc_barrier.mark_blocked_region_enter();
        assert_eq!(shared.mem.gc_barrier.blocked_count(), 1);

        // `ForeignCallGuard::enter` resolves the process-global cell (see its
        // `process_vm()` call), which `PROCESS_VM_TEST_LOCK` does not protect --
        // `Vm::new` republishes it from test sites that never take this lock.
        // A theft here sends the blocked-region LEAVE to another VM's barrier
        // and ours stays at 1: `left: 1, right: 0` on "outermost call must
        // leave the blocked region", 1 of 30 full-suite runs.
        if !process_vm_cell_holds(&our_weak) {
            return;
        }

        {
            // Outermost call → leave blocked region, counted mutator.
            let _fg = ForeignCallGuard::enter();
            assert_eq!(
                shared.mem.gc_barrier.blocked_count(),
                0,
                "outermost call must leave the blocked region"
            );
            assert!(!with_foreign_thread(|jt| jt
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire))
            .unwrap());
            assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 1);
            {
                // Nested call (e.g. a JNI up-call) → depth only, no transition.
                let _fg2 = ForeignCallGuard::enter();
                assert_eq!(shared.mem.gc_barrier.blocked_count(), 0);
                assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 2);
            }
            assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 1);
        }
        // Outermost return → idle/blocked again.
        assert_eq!(
            shared.mem.gc_barrier.blocked_count(),
            1,
            "outermost return must re-enter the blocked region"
        );
        assert!(with_foreign_thread(|jt| jt
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire))
        .unwrap());
        assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 0);

        // A non-foreign thread sees an entirely inert guard.
        let tid = with_foreign_thread(|jt| jt.thread_id).unwrap();
        shared.threads.thread_registry.mark_dead(tid);
        shared.mem.gc_barrier.mark_blocked_region_leave();
        assert!(detach_foreign_thread(&shared));
        {
            let _fg = ForeignCallGuard::enter();
            assert_eq!(shared.mem.gc_barrier.blocked_count(), 0);
            assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 0);
        }
    }

    /// `host_thread_enter_native` excludes an idle (non-foreign) coordinator
    /// thread from stop-the-world, so a GC initiated by another thread does not
    /// wait for it — the creating-thread STW-hang the primitive resolves.
    #[test]
    fn host_native_excludes_idle_thread_from_stw() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::collections::HashMap;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let our_weak = Arc::downgrade(&shared);
        set_process_vm(&shared);

        let init = shared.threads.thread_registry.next_thread_id();
        shared.threads.thread_registry.register(init, "init", None);
        let coord = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(coord, "coordinator", None);
        assert_eq!(shared.threads.thread_registry.alive_count(), 2);

        // `host_thread_enter_native` resolves the PROCESS-GLOBAL cell, not the
        // `shared` above, and `PROCESS_VM_TEST_LOCK` does not protect it:
        // `Vm::new` republishes that cell unconditionally, from 60+ test call
        // sites that never take this lock (see `process_vm_cell_holds`). When
        // one of them lands in this window, the call below marks a blocked
        // region on THEIR barrier and the assertion reads ours, which is still
        // 0 -- measured as `left: 0, right: 1`, 2 of 20 full-suite runs.
        //
        // Checked BEFORE the call, not just before the assertion, so a lost
        // cell also means we never perturb the other test's barrier.
        if !process_vm_cell_holds(&our_weak) {
            return;
        }

        // This thread declares itself in-native (no foreign attachment present).
        assert!(host_thread_enter_native());
        assert_eq!(shared.mem.gc_barrier.blocked_count(), 1);

        // A STW from the initiator excludes the in-native thread → waits for nobody.
        assert!(shared.mem.gc_barrier.request_stw(init, 2));
        assert_eq!(
            shared.mem.gc_barrier.pending_count(),
            0,
            "in-native thread must be excluded from the STW expected-set"
        );
        shared.mem.gc_barrier.wait_for_all();
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());

        assert!(host_thread_leave_native());
        // Same re-check as the twin below: a republish mid-body sends the
        // `leave` to another VM's barrier and leaves ours at 1.
        if !process_vm_cell_holds(&our_weak) {
            return;
        }
        assert_eq!(shared.mem.gc_barrier.blocked_count(), 0);
    }

    /// CENSUS-RECONCILE — the IDENTITY-census twin of the test above, and the
    /// one that fails without the fix.
    ///
    /// `host_native_excludes_idle_thread_from_stw` drives `request_stw`, the
    /// LEGACY path, whose `expected` subtracts the barrier's anonymous
    /// `threads_blocked` counter — the one thing `host_thread_enter_native`
    /// always bumped. Production does not use that path: it computes `expected`
    /// from `alive_count_blocked_and_os_tids` (the `in_blocked_region` identity
    /// census) via `request_stw_counted_with_live_blocked`
    /// (`runtime/interpreter.rs`). Before the fix this test observes
    /// `blocked == 0`, `expected == 1` and a `pending_count()` of 1 — an
    /// `expected` slot for a thread parked in host code that will never arrive,
    /// i.e. the hang the primitive exists to prevent, reached through the only
    /// census production actually consults.
    ///
    /// Windows/Linux only: the identity link from a host thread back to its
    /// registry entry is the published `os_tid`, and `set_os_tid_current` has a
    /// backend only on those two platforms (see
    /// `ThreadRegistry::thread_id_for_current_os_tid`).
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn host_native_excludes_idle_thread_from_the_identity_census() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::collections::HashMap;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let our_weak = Arc::downgrade(&shared);
        set_process_vm(&shared);

        let init = shared.threads.thread_registry.next_thread_id();
        shared.threads.thread_registry.register(init, "init", None);
        let coord = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(coord, "coordinator", None);
        // THIS OS thread is the coordinator's carrier. Publishing its OS id is
        // what lets `host_thread_enter_native` name itself to the identity
        // census; the real creating thread gets the same publication from
        // `Vm::new` (`vm/src/vm/vm_init.rs`, `set_os_tid_current(ThreadId(0))`).
        shared.threads.thread_registry.set_os_tid_current(coord);
        assert!(!is_foreign_attached());
        assert_eq!(shared.threads.thread_registry.alive_count(), 2);

        // Same process-cell hazard as the twin above: `host_thread_enter_native`
        // resolves the global cell, and `Vm::new` republishes it from test call
        // sites that never take this lock. Bail before touching the barrier.
        if !process_vm_cell_holds(&our_weak) {
            return;
        }

        assert!(host_thread_enter_native());
        assert_eq!(shared.mem.gc_barrier.blocked_count(), 1);

        // The production census — NOT the legacy anonymous counter.
        let (alive, blocked, _tids, blocked_tids) = shared
            .threads
            .thread_registry
            .alive_count_blocked_and_os_tids();
        assert_eq!(alive, 2);
        assert_eq!(
            blocked, 1,
            "a host-native thread must be visible to the IDENTITY census that \
             computes `expected`, not only to the anonymous counter",
        );
        assert!(
            blocked_tids.contains(&coord.0),
            "the excluded identity must be the coordinator's: {blocked_tids:?}",
        );

        // Same call shape as `runtime/interpreter.rs`'s GC initiator.
        let requested = shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(init, || {
                (alive as u32, blocked as u32, blocked_tids)
            });
        assert!(requested);
        assert_eq!(
            shared.mem.gc_barrier.pending_count(),
            0,
            "the host-native thread must not occupy an `expected` slot it can \
             never arrive to fill",
        );
        shared.mem.gc_barrier.wait_for_all(); // returns immediately — no hang
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());

        assert!(host_thread_leave_native());
        // Re-checked, not assumed: the cell can be republished at any point in
        // this body, and `host_thread_leave_native` then decrements the NEW
        // owner's barrier instead of ours, leaving ours at 1. That is the
        // `left: 1, right: 0` seen here in 1 of 24 full-suite runs — the same
        // theft as the guard above, observed one assertion later.
        if !process_vm_cell_holds(&our_weak) {
            return;
        }
        assert_eq!(shared.mem.gc_barrier.blocked_count(), 0);
        assert!(
            !shared.threads.thread_registry.is_blocked(coord),
            "leaving must clear the identity flag too — a thread that resumes \
             with it raised is excluded from every LATER pause while running",
        );
        let (_, blocked_after, _, _) = shared
            .threads
            .thread_registry
            .alive_count_blocked_and_os_tids();
        assert_eq!(
            blocked_after, 0,
            "the census must see the thread running again"
        );
    }

    #[test]
    fn jni_version_constant() {
        assert_eq!(JNI_VERSION_1_8, 0x00010008);
    }

    /// A nested native call must not leave the ENCLOSING one without a JNI
    /// context.
    ///
    /// This is the unit-level statement of
    /// `nested-jni-call-cleared-the-enclosing-natives-context-FIXED-20260828.md`:
    /// `set_jni_context` + an unconditional `clear_jni_context` on exit is
    /// wrong for a call that nests, and the shape nests whenever a native calls
    /// back into Java. The failure is silent — `with_jni_context` answers
    /// `None` and every JNI up-call the outer native makes afterwards hands the
    /// host library a null `jobject` — so a test that only checks the OUTERMOST
    /// exit (which the old code got right) cannot see it.
    ///
    /// Asserted on the thread-locals directly rather than through a real JNI
    /// dispatch, because reproducing the nesting for real needs a foreign
    /// library that calls back into Java; that end of it is
    /// `probes/OpenSslTls13ReentryBisectProbe.java`.
    #[test]
    fn nested_native_call_restores_the_enclosing_jni_context() {
        use crate::config::VmConfig;
        use crate::vm::Vm;

        let vm = Vm::new(VmConfig::default());
        let _jni = JniTls::cleanup_only();
        // Nothing installed to begin with, which is the state the OUTERMOST
        // exit must restore.
        restore_jni_context(None);
        assert!(
            with_shared_vm(|_| ()).is_none(),
            "precondition: no context installed"
        );

        // Outer native call.
        let outer_prev = replace_jni_context(&vm.shared);
        assert!(
            outer_prev.is_none(),
            "outer saw a context that was not there"
        );
        assert!(
            with_shared_vm(|_| ()).is_some(),
            "outer install did not take"
        );

        {
            // Java upcall -> a SECOND native call on the same thread.
            let inner_prev = replace_jni_context(&vm.shared);
            assert!(
                inner_prev.is_some(),
                "the inner call must SEE the outer context, not a cleared slot"
            );
            restore_jni_context(inner_prev);
        }

        // The whole point: the outer native is still running.
        assert!(
            with_shared_vm(|_| ()).is_some(),
            "the inner call's exit cleared the ENCLOSING native's context — this is \
             the TLSv1.3 client-certificate defect, and it is silent: the outer \
             native's next up-call would be answered as if the VM were detached"
        );

        // Outermost exit still releases, exactly as `clear_jni_context` did.
        restore_jni_context(outer_prev);
        assert!(
            with_shared_vm(|_| ()).is_none(),
            "the outermost exit must leave no context behind"
        );
    }

    /// JNI `ThrowNew` must materialise the exact supplied throwable class and
    /// its message, rather than using the historical generic sentinel.  This
    /// is the contract jnr-ffi/posix relies on when it catches a native-load
    /// failure and selects its Java fallback provider.
    #[test]
    fn jni_throw_new_preserves_class_message_and_gc_root() {
        use crate::config::VmConfig;
        use crate::vm::Vm;

        let mut vm = Vm::new(VmConfig::default());
        let class_id = vm
            .shared
            .load_class_concurrent("java/lang/IllegalArgumentException")
            .expect("load IllegalArgumentException");
        let message = CString::new("native provider unavailable").unwrap();

        let _jni = JniTls::replace(&vm.shared).with_thread(vm.main_thread.as_mut() as *mut _);
        assert_eq!(
            jni_throw_new(
                get_jni_env(),
                class_id_to_jclass(class_id),
                message.as_ptr(),
            ),
            JNI_OK
        );

        let occurred = jni_exception_occurred(get_jni_env());
        assert_ne!(occurred, 0, "ThrowNew must set a pending exception");
        let exception = vm
            .main_thread
            .native_pending_return
            .expect("ThrowNew must publish a GC-rooted exception");
        assert_eq!(
            vm.shared.mem.heap.class_id_of(exception),
            class_id,
            "ThrowNew must preserve the supplied jclass"
        );

        let Value::Object(Some(message_object)) = vm.shared.mem.heap.get_field(exception, 1) else {
            panic!("ThrowNew exception must retain its detailMessage");
        };
        assert_eq!(
            read_java_string(&vm.shared.mem.heap, message_object).as_deref(),
            Some("native provider unavailable")
        );

        jni_exception_clear(get_jni_env());
        assert_eq!(jni_exception_occurred(get_jni_env()), 0);
        assert!(vm.main_thread.native_pending_return.is_none());
    }

    /// Regression for the `AttachCurrentThread`/`GetEnv` JNIEnv* indirection bug:
    /// `write_jni_env` must hand back `get_jni_env()` (a pointer-to-pointer-to
    /// function-table) so `(*env)[slot]` resolves a function — NOT the
    /// table-array pointer `JNI_TABLE_PTR.load()`, which is one indirection too
    /// shallow and made `(*env)[slot]` read a function's code bytes as a slot
    /// pointer (jump-to-garbage, the first crash the foreign-attach soak hit).
    #[test]
    fn attach_env_indirection_is_correct() {
        use std::sync::atomic::Ordering;
        // The env `write_jni_env` produces must equal the canonical `get_jni_env`.
        let canonical = get_jni_env();
        let mut penv: *mut std::ffi::c_void = std::ptr::null_mut();
        write_jni_env(&mut penv as *mut *mut std::ffi::c_void);
        assert_eq!(
            penv as usize, canonical as usize,
            "attach env must equal get_jni_env()"
        );

        // It must be ONE level above the table-array pointer (the old-bug value).
        let table_value = JNI_TABLE_PTR.load(Ordering::Acquire) as usize;
        assert_ne!(
            penv as usize, table_value,
            "JNIEnv* must be &JNI_TABLE_PTR, not the loaded table-array pointer"
        );

        // Double-deref + call GetVersion (function-table slot 4) to prove the
        // indirection level resolves a real function (the bug jumped to garbage).
        let env = penv as JNIEnv;
        let slot = unsafe { *(*env).add(4) };
        assert_ne!(slot, 0);
        let get_version: extern "C" fn(JNIEnv) -> JInt = unsafe { std::mem::transmute(slot) };
        assert_eq!(get_version(env), JNI_VERSION_1_8);
    }

    /// Interpreter round i1 wave 11, lane L1: `GetEnv` for an interface the
    /// VM does not provide answers `JNI_EVERSION` and a NULL env; it handed
    /// back the `JNIEnv*`, whose slots a JVMTI caller would call as JVMTI
    /// functions. Wave 12: JVMTI 1.2 is provided in `experimental-debug`
    /// builds (a C `jvmtiEnv`, not the `JNIEnv*`); JVMDI never is. A JNI
    /// version still answers the `JNIEnv*`.
    #[test]
    fn get_env_refuses_a_jvmti_version() {
        const JVMTI_VERSION_1_2: JInt = 0x3001_0200;
        const JVMDI_VERSION_1: JInt = 0x2001_0000;
        let mut penv = std::ptr::NonNull::<std::ffi::c_void>::dangling().as_ptr();
        let rc = jni_get_env(
            std::ptr::null(),
            &mut penv as *mut *mut std::ffi::c_void,
            JVMDI_VERSION_1,
        );
        assert_eq!(rc, JNI_EVERSION);
        assert!(penv.is_null(), "no env for an interface the VM lacks");
        let rc = jni_get_env(
            std::ptr::null(),
            &mut penv as *mut *mut std::ffi::c_void,
            JVMTI_VERSION_1_2,
        );
        if cfg!(feature = "experimental-debug") {
            assert_eq!(rc, JNI_OK);
            assert!(!penv.is_null());
            assert_ne!(penv as usize, get_jni_env() as usize, "not the JNI table");
        } else {
            assert_eq!(rc, JNI_EVERSION);
            assert!(penv.is_null());
        }
        let rc = jni_get_env(
            std::ptr::null(),
            &mut penv as *mut *mut std::ffi::c_void,
            JNI_VERSION_1_8,
        );
        assert_eq!(rc, JNI_OK);
        assert_eq!(penv as usize, get_jni_env() as usize);
        // Wave 14: a JNI version HotSpot does not provide (1.3, 99) is
        // refused too; the ones it does (1.1, 24) are served.
        for unsupported in [0x0001_0003, 0x0063_0000] {
            let rc = jni_get_env(
                std::ptr::null(),
                &mut penv as *mut *mut std::ffi::c_void,
                unsupported,
            );
            assert_eq!(rc, JNI_EVERSION, "version {unsupported:#x}");
            assert!(penv.is_null());
        }
        for supported in [0x0001_0001, 0x0018_0000] {
            let rc = jni_get_env(
                std::ptr::null(),
                &mut penv as *mut *mut std::ffi::c_void,
                supported,
            );
            assert_eq!(rc, JNI_OK, "version {supported:#x}");
            assert_eq!(penv as usize, get_jni_env() as usize);
        }
    }

    #[test]
    fn jni_function_table_get_version() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(4) };
        assert_ne!(func_ptr, 0);
        let get_version: extern "C" fn(JNIEnv) -> JInt = unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(get_version(env), JNI_VERSION_1_8);
    }

    #[test]
    fn jni_function_table_exception_check() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(228) };
        let exception_check: extern "C" fn(JNIEnv) -> JBoolean =
            unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(exception_check(env), JNI_FALSE);
    }

    #[test]
    fn jni_function_table_is_same_object() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj1 = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let obj2 = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let local1 = obj_to_jobject(obj1);
        let local2 = obj_to_jobject(obj2);
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        // Slot 24 per `jni.h`, not 25: this test read the slot the table
        // happened to use rather than the slot a compiled native loads, so it
        // stayed green through the whole 19..27 shift it was best placed to
        // catch. `jni_function_table_matches_the_header` now pins the layout.
        let func_ptr = unsafe { *(*env).add(24) };
        let is_same: extern "C" fn(JNIEnv, JObject, JObject) -> JBoolean =
            unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(is_same(env, local1, local1), JNI_TRUE);
        assert_eq!(is_same(env, local1, local2), JNI_FALSE);
        assert_eq!(is_same(env, 0, 0), JNI_TRUE); // null == null
    }

    #[test]
    fn jni_exception_check_reports_the_pending_slot() {
        // It used to be a constant `JNI_FALSE`, which made every
        // `if (ExceptionCheck(env))` branch in every native library dead code.
        let env = get_jni_env();
        let _ = take_jni_pending_exception();
        assert_eq!(
            jni_exception_check(env),
            JNI_FALSE,
            "nothing pending, nothing to report"
        );
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(0x1234));
        assert_eq!(
            jni_exception_check(env),
            JNI_TRUE,
            "a pending exception must be visible to the native that caused it"
        );
        // And `ExceptionClear` is what turns it off again.
        jni_exception_clear(env);
        assert_eq!(jni_exception_check(env), JNI_FALSE);
        let _ = take_jni_pending_exception();
    }

    #[test]
    fn jni_exception_describe_clears_the_pending_slot() {
        // Without a VM context there is no throwable to print, but the clear
        // — the half that decides whether the exception reaches the caller —
        // must still happen.
        let env = get_jni_env();
        let _ = take_jni_pending_exception();
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(0x1234));
        jni_exception_describe(env);
        assert_eq!(
            jni_exception_check(env),
            JNI_FALSE,
            "ExceptionDescribe leaves nothing pending"
        );
        let _ = take_jni_pending_exception();
    }

    #[test]
    fn jni_function_table_stub_returns_zero() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(0) };
        let stub: extern "C" fn() -> usize = unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(stub(), 0);
    }

    #[test]
    fn global_refs_multiple_entries() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj1 = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let obj2 = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let h1 = refs.add(obj1);
        let h2 = refs.add(obj2);
        assert_ne!(h1, h2);
        assert_eq!(refs.count(), 2);
        // Each handle resolves to the correct object.
        assert_eq!(refs.resolve(h1), Some(obj1));
        assert_eq!(refs.resolve(h2), Some(obj2));
    }

    #[test]
    fn global_refs_remove_unknown_handle_returns_false() {
        let mut refs = JniGlobalRefs::new();
        // 0xDEAD_BEEF1 has bit 0 set (looks like a global-ref handle) but was never added.
        assert!(!refs.remove(0xDEAD_BEEF1));
        // A local-ref handle (bit 0 = 0) also returns false.
        assert!(!refs.remove(8));
    }

    #[test]
    fn global_refs_default_trait() {
        let refs = JniGlobalRefs::default();
        assert_eq!(refs.count(), 0);
    }

    #[test]
    fn local_frame_default_trait() {
        let frame = JniLocalFrame::default();
        assert_eq!(frame.refs.len(), 0);
    }

    #[test]
    fn method_id_encode_decode() {
        let class_id = ClassId::new(42);
        let method_index = 7u16;
        let mid = encode_method_id(class_id, method_index);
        let (decoded_class, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_class, class_id);
        assert_eq!(decoded_idx, method_index);
    }

    #[test]
    fn field_id_encode_decode() {
        let class_id = ClassId::new(100);
        let field_index = 3usize;
        let fid = encode_field_id(class_id, field_index);
        let (decoded_class, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_class, class_id);
        assert_eq!(decoded_idx, field_index);
    }

    #[test]
    fn jni_get_array_length_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let arr = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 10);
        let jarray = obj_to_jobject(arr);
        let env = get_jni_env();
        let len = jni_get_array_length(env, jarray);
        assert_eq!(len, 10);
    }

    #[test]
    fn jni_field_access_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
        let jobj = obj_to_jobject(obj);
        let fid = encode_field_id(ClassId::new(0), 1);
        let env = get_jni_env();
        jni_set_int_field(env, jobj, fid, 42);
        let val = jni_get_int_field(env, jobj, fid);
        assert_eq!(val, 42);
    }

    #[test]
    fn jni_new_int_array_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 5);
        assert_ne!(arr, 0);
        let len = jni_get_array_length(env, arr);
        assert_eq!(len, 5);
    }

    /// Round 12 wave 6 (lane jni): `DefineClass` defines through the caller's
    /// loader in both modes (the owner turned it on for `--compatible` too,
    /// 2026-09-27), unless the kill switch is off. Either way a failed define answers NULL with something pending.
    #[test]
    fn jni_define_class_policy_follows_the_vm_mode() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let not_a_class: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let compatible = Arc::new(SharedVm::new(VmConfig::default()));
        {
            let _jni = JniTls::context(&compatible);
            assert_eq!(
                jni_define_class_through_loader_active(),
                jni_define_class_loader_switch()
            );
            let _ = take_jni_pending_exception();
            let env = get_jni_env();
            let c = jni_define_class(env, std::ptr::null(), 0, not_a_class.as_ptr(), 4);
            assert_eq!(c, 0);
            assert!(take_jni_pending_exception().is_some());
        }
        let mut config = VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        config.use_synthetic_jdk = false;
        let jdk_only = Arc::new(SharedVm::new(config));
        let _jni = JniTls::context(&jdk_only);
        assert_eq!(
            jni_define_class_through_loader_active(),
            jni_define_class_loader_switch()
        );
        let upcall_switch = !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JNI_UPCALL_TYPED_ERRORS")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("0" | "false" | "off" | "no")
        );
        assert_eq!(jni_types_vm_errors(&jdk_only), upcall_switch);
        let _ = take_jni_pending_exception();
        let env = get_jni_env();
        // No attached `JvmThread` here: the loader path declines and the
        // legacy define answers.
        let c = jni_define_class(env, std::ptr::null(), 0, not_a_class.as_ptr(), 4);
        assert_eq!(c, 0);
        assert!(take_jni_pending_exception().is_some());
        let c = jni_define_class(env, std::ptr::null(), 0, not_a_class.as_ptr(), 0);
        assert_eq!(c, 0);
        assert!(take_jni_pending_exception().is_some());
    }

    /// Round 12 wave 6 (lane jni): `GetSuperclass` of an interface is NULL,
    /// though its class file (and the class store) names `java/lang/Object`.
    #[test]
    fn jni_get_superclass_of_an_interface_is_null() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        // Both modes answer NULL (owner decision 2026-09-27); strict here.
        let mut config = VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        config.use_synthetic_jdk = false;
        let shared = Arc::new(SharedVm::new(config));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        // Only meaningful where this bare VM can load the interface.
        let Ok(runnable) = shared.load_class_concurrent("java/lang/Runnable") else {
            return;
        };
        let is_interface = shared
            .classes
            .class_manager
            .read()
            .get_class(runnable)
            .is_some_and(|c| c.is_interface());
        if !is_interface {
            return;
        }
        assert_eq!(jni_get_superclass(env, class_id_to_jclass(runnable)), 0);
    }

    /// Round 12 wave 7 (lane compat): `FindClass` leaves an exception pending
    /// when it answers NULL (a miss, a NULL name, a dotted name), and finds a
    /// class by its internal name, in BOTH modes (owner decision 2026-09-27).
    /// With no Java caller the application-namespace lookup answers, as
    /// HotSpot's system-loader default does.
    #[test]
    fn jni_find_class_raises_on_a_miss_in_both_modes() {
        use crate::config::VmConfig;
        use cratonvm_types::compat::CompatibilityMode;
        let raises = jni_find_class_raises();
        let loader_aware = jni_find_class_loader_aware();
        let missing = CString::new("no/such/R12CompatFindClass").unwrap();
        let object = CString::new("java/lang/Object").unwrap();
        let dotted = CString::new("java.lang.Object").unwrap();
        for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
            let mut config = VmConfig::default();
            config.compatibility_mode = mode;
            if mode.is_jdk_only() {
                config.use_synthetic_jdk = false;
            }
            let shared = Arc::new(SharedVm::new(config));
            // No attached `JvmThread` (as in the `DefineClass` test above): the
            // caller-context path declines, the application-namespace lookup
            // answers, and a failure leaves the pending sentinel.
            let _jni = JniTls::context(&shared);
            let env = get_jni_env();
            let _ = take_jni_pending_exception();

            assert_eq!(jni_find_class(env, missing.as_ptr()), 0, "{mode:?}");
            assert_eq!(take_jni_pending_exception().is_some(), raises, "{mode:?} miss");
            assert_eq!(jni_find_class(env, std::ptr::null()), 0, "{mode:?}");
            assert_eq!(take_jni_pending_exception().is_some(), raises, "{mode:?} NULL");

            // Only meaningful where this bare VM can load the class.
            if shared.load_class_concurrent("java/lang/Object").is_ok() {
                assert_ne!(jni_find_class(env, object.as_ptr()), 0, "{mode:?}");
                assert!(take_jni_pending_exception().is_none(), "{mode:?} hit");
                let d = jni_find_class(env, dotted.as_ptr());
                if loader_aware {
                    assert_eq!(d, 0, "{mode:?}: a dotted name names no class");
                    assert_eq!(take_jni_pending_exception().is_some(), raises, "{mode:?}");
                } else {
                    let _ = take_jni_pending_exception();
                }
            }
        }
    }

    /// Round 12 wave 8 (lane compat2): `FindClass` returns an INITIALIZED
    /// class, as HotSpot's `jni_FindClass` (`init = true`) does, in BOTH modes;
    /// with no attached `JvmThread` nothing can run `<clinit>`, and the class
    /// stays as it was.
    #[test]
    fn jni_find_class_initializes_what_it_finds_in_both_modes() {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use cratonvm_types::compat::CompatibilityMode;
        let initializes = jni_find_class_initializes();
        // Interfaces with no `<clinit>` in any image, so initializing one in
        // this bare VM (no `System` init) runs no Java that could fail.
        const CANDIDATES: [&str; 4] = [
            "java/util/RandomAccess",
            "java/lang/Runnable",
            "java/util/function/Supplier",
            "java/lang/AutoCloseable",
        ];
        for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
            let mut config = VmConfig::default();
            config.compatibility_mode = mode;
            if mode.is_jdk_only() {
                config.use_synthetic_jdk = false;
            }
            let shared = Arc::new(SharedVm::new(config));
            // A class this bare VM can load and has not initialized yet.
            let Some((name, cid)) = CANDIDATES.iter().find_map(|name| {
                let cid = shared.load_class_concurrent(name).ok()?;
                (!crate::vm::is_class_initialized_via_manager(&shared, cid)).then_some((*name, cid))
            }) else {
                continue;
            };
            let name = CString::new(name).unwrap();

            // No thread: found, left as it was.
            {
                let _jni = JniTls::context(&shared);
                let env = get_jni_env();
                assert_ne!(jni_find_class(env, name.as_ptr()), 0, "{mode:?}");
                assert!(
                    !crate::vm::is_class_initialized_via_manager(&shared, cid),
                    "{mode:?}: no JNI thread, so no `<clinit>` ran"
                );
            }

            let mut thread = JvmThread::new(ThreadId(0), "r12-compat2-findclass");
            let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
            let env = get_jni_env();
            let _ = take_jni_pending_exception();
            assert_ne!(jni_find_class(env, name.as_ptr()), 0, "{mode:?}");
            assert!(take_jni_pending_exception().is_none(), "{mode:?}");
            // Before this wave FindClass never initialized: `false` here.
            assert_eq!(
                crate::vm::is_class_initialized_via_manager(&shared, cid),
                initializes,
                "{mode:?}: FindClass initializes the class it returns"
            );
        }
    }

    #[test]
    fn jni_array_region_roundtrip() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 4);
        let data: [JInt; 4] = [10, 20, 30, 40];
        jni_set_int_array_region(env, arr, 0, 4, data.as_ptr());
        let mut out = [0i32; 4];
        jni_get_int_array_region(env, arr, 0, 4, out.as_mut_ptr());
        assert_eq!(out, [10, 20, 30, 40]);
    }

    #[test]
    fn region_bounds_ok_rejects_out_of_range() {
        // In-bounds / empty windows accept.
        assert!(region_bounds_ok(0, 4, 4));
        assert!(region_bounds_ok(4, 0, 4));
        assert!(region_bounds_ok(2, 2, 4));
        // Drain any pending sentinel set by the rejecting cases below so this
        // test doesn't leak into the shared thread-local.
        let _ = take_jni_pending_exception();
        // Out-of-range windows reject and flag a pending exception.
        assert!(!region_bounds_ok(3, 2, 4)); // start+len = 5 > 4
        assert!(take_jni_pending_exception().is_some());
        assert!(!region_bounds_ok(5, 0, 4)); // start past end
        let _ = take_jni_pending_exception();
        // Overflow can't wrap into a spuriously-valid range.
        assert!(!region_bounds_ok(JSize::MAX, JSize::MAX, 4));
        let _ = take_jni_pending_exception();
    }

    #[test]
    fn jni_get_array_region_out_of_bounds_no_partial_write() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 4);
        let data: [JInt; 4] = [10, 20, 30, 40];
        jni_set_int_array_region(env, arr, 0, 4, data.as_ptr());
        // Request a region that runs off the end: start=2 len=4 on a length-4
        // array. The destination must be left fully untouched (no partial copy)
        // and a pending exception must be flagged.
        let _ = take_jni_pending_exception();
        let mut out = [-1i32; 4];
        jni_get_int_array_region(env, arr, 2, 4, out.as_mut_ptr());
        assert_eq!(out, [-1, -1, -1, -1], "OOB region must not write");
        assert!(take_jni_pending_exception().is_some());

        // An OOB set must not corrupt the array either.
        let bad: [JInt; 4] = [99, 99, 99, 99];
        jni_set_int_array_region(env, arr, 2, 4, bad.as_ptr());
        let _ = take_jni_pending_exception();
        let mut back = [0i32; 4];
        jni_get_int_array_region(env, arr, 0, 4, back.as_mut_ptr());
        assert_eq!(back, [10, 20, 30, 40], "OOB set must not mutate array");
    }

    #[test]
    fn jni_java_vm_get_env() {
        let env = get_jni_env();
        let mut vm_ptr: JavaVM = std::ptr::null();
        let result = jni_get_java_vm(env, &mut vm_ptr);
        assert_eq!(result, JNI_OK);
        assert!(!vm_ptr.is_null());
    }

    #[test]
    fn jni_invoke_table_get_env() {
        let vm = get_java_vm();
        let func_ptr = unsafe { *(*vm).add(6) };
        assert_ne!(func_ptr, 0);
    }

    // -----------------------------------------------------------------------
    // M2: RegisterNatives / find_jni_native tests
    // -----------------------------------------------------------------------

    #[test]
    fn register_and_find_jni_native() {
        // Register a fake function pointer and verify lookup works.
        extern "C" fn fake_native(_env: JNIEnv, _this: JObject) -> u64 {
            42
        }
        let fn_ptr = fake_native as *const () as usize;
        let table = JniNativeMethodTable::default();
        register_jni_native_in(&table, "com/example/Foo", "bar", "()J", fn_ptr);
        assert_eq!(
            find_jni_native_in(&table, "com/example/Foo", "bar", "()J"),
            Some(fn_ptr)
        );
        // Different class → not found
        assert!(find_jni_native_in(&table, "com/example/Other", "bar", "()J").is_none());
        // Different descriptor → not found
        assert!(find_jni_native_in(&table, "com/example/Foo", "bar", "()V").is_none());
    }

    /// gce e1/j: the JNI phase census times nothing and prints nothing when
    /// off; on, it prints each phase that ran, in phase order, as
    /// `name=avg_ns/count`.
    #[test]
    fn jni_phase_census_is_silent_off_and_names_each_phase_on() {
        let off = JniPhaseCensus::new(false);
        let mut clock = off.start();
        assert!(clock.is_none());
        off.lap(JniPhase::Deposit, &mut clock);
        off.stop(JniPhase::NativeBody, clock);
        assert_eq!(off.line(), None);
        assert!(deposit_phase_clock(&off).is_none());

        let on = JniPhaseCensus::new(true);
        let mut clock = on.start();
        assert!(clock.is_some());
        on.lap(JniPhase::Deposit, &mut clock);
        on.lap(JniPhase::Deposit, &mut clock);
        on.stop(JniPhase::NativeBody, clock);
        let line = on.line().expect("on");
        assert!(line.starts_with("[GC] jni_phase:"), "{line}");
        let deposit = line.find(" deposit=").expect("deposit phase");
        let body = line.find(" native_body=").expect("native_body phase");
        assert!(deposit < body, "phase order: {line}");
        assert!(line.contains("ns/2 "), "two deposit laps: {line}");
        assert!(line.ends_with("ns/1"), "one native_body stop: {line}");
        assert!(!line.contains("lookup="), "a phase that never ran is omitted: {line}");
        // Outside the package's in-native deposit the sub-phase clock stays off.
        assert!(deposit_phase_clock(&on).is_none());
        assert_eq!(JniPhase::NAMES.len(), JniPhase::COUNT);
    }

    /// gce e1/j: `find_jni_native`'s per-thread memo answers only for the same
    /// realm, the same triple and the same registration generation: a
    /// re-registration, an unregistration and another VM all see the table.
    #[test]
    fn find_jni_native_memo_follows_the_registration_generation() {
        extern "C" fn first(_env: JNIEnv, _this: JObject) -> u64 {
            1
        }
        extern "C" fn second(_env: JNIEnv, _this: JObject) -> u64 {
            2
        }
        let (p1, p2) = (first as *const () as usize, second as *const () as usize);
        let a = SharedVm::new(crate::config::VmConfig::default());
        let b = SharedVm::new(crate::config::VmConfig::default());
        let find = |vm: &SharedVm, d: &str| find_jni_native(&vm.natives, "t/Memo", "f", d);
        assert_eq!(find(&a, "()J"), None, "a miss is not memoised");
        register_jni_native(&a.natives, "t/Memo", "f", "()J", p1);
        assert_eq!(find(&a, "()J"), Some(p1));
        assert_eq!(find(&a, "()J"), Some(p1), "served by the memo");
        assert_eq!(find(&b, "()J"), None, "another VM's realm");
        assert_eq!(find(&a, "()V"), None, "another descriptor");
        register_jni_native(&a.natives, "t/Memo", "f", "()J", p2);
        assert_eq!(find(&a, "()J"), Some(p2), "a re-registration voids the memo");
        // The unregistration's shape: a table write, then the bump.
        a.natives.jni_native_methods.write().clear();
        a.natives
            .jni_native_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        assert_eq!(find(&a, "()J"), None, "an unregistration voids the memo");
    }

    /// The point of moving this table off a process global (JDK-ONLY-WAVE2 §6,
    /// 2026-08-06): two VMs in one process must not see each other's
    /// `RegisterNatives`. While the table was a `static`, this test could not
    /// even be written — there was one table and the second `register` would
    /// have been visible to the first lookup.
    #[test]
    fn jni_native_tables_are_isolated_per_vm() {
        extern "C" fn a(_env: JNIEnv, _this: JObject) -> u64 {
            1
        }
        extern "C" fn b(_env: JNIEnv, _this: JObject) -> u64 {
            2
        }
        let (vm_a, vm_b) = (
            JniNativeMethodTable::default(),
            JniNativeMethodTable::default(),
        );

        register_jni_native_in(
            &vm_a,
            "com/example/Iso",
            "m",
            "()J",
            a as *const () as usize,
        );

        // VM B has not bound it, and must not inherit VM A's binding.
        assert!(
            find_jni_native_in(&vm_b, "com/example/Iso", "m", "()J").is_none(),
            "VM B saw a RegisterNatives that only VM A performed"
        );

        // And once B binds its own, the two answer differently for one triple.
        register_jni_native_in(
            &vm_b,
            "com/example/Iso",
            "m",
            "()J",
            b as *const () as usize,
        );
        assert_eq!(
            find_jni_native_in(&vm_a, "com/example/Iso", "m", "()J"),
            Some(a as *const () as usize)
        );
        assert_eq!(
            find_jni_native_in(&vm_b, "com/example/Iso", "m", "()J"),
            Some(b as *const () as usize)
        );
    }

    /// JNI spec 11.3 name mangling, pinned against symbols a real toolchain
    /// emits. The `$` case is the one that was broken: `jni_encode` passed
    /// every ASCII character through unescaped, so no native on a NESTED class
    /// could ever be found by `dlsym` — the symbol in the library is
    /// `Java_JdkOnlyPlatformProbe_00024JniProbe_add` (confirmed with `nm -D` on
    /// the library built by probes/jdkonly_jni_probe.c, which HotSpot binds
    /// from the same file) and the VM looked up
    /// `Java_JdkOnlyPlatformProbe$JniProbe_add`.
    ///
    /// Asserting the FULL expected symbol, not "contains _00024": a mangler
    /// that escaped `$` and also mangled something else would still pass a
    /// containment check.
    #[test]
    fn jni_mangling_escapes_every_non_alphanumeric() {
        // Nested class — the regression.
        assert_eq!(
            jni_short_name("JdkOnlyPlatformProbe$JniProbe", "add"),
            "Java_JdkOnlyPlatformProbe_00024JniProbe_add"
        );
        // Package separator, and the ordinary case still unchanged.
        assert_eq!(
            jni_short_name("java/lang/System", "arraycopy"),
            "Java_java_lang_System_arraycopy"
        );
        // Underscore in a method name is `_1`, and it must not collide with
        // the `/`→`_` rule.
        assert_eq!(
            jni_short_name("com/example/Foo_Bar", "do_it"),
            "Java_com_example_Foo_1Bar_do_1it"
        );
        // Doubly nested.
        assert_eq!(jni_short_name("a/B$C$D", "m"), "Java_a_B_00024C_00024D_m");
        // Long form: the parameter block carries `;` → `_2` and `[` → `_3`,
        // and a nested parameter type takes the `$` escape too.
        assert_eq!(
            jni_long_name("a/B$C", "m", "(Ljava/lang/String;[ILa/B$C;)V"),
            "Java_a_B_00024C_m__Ljava_lang_String_2_3ILa_B_00024C_2"
        );
        // Non-ASCII still takes the same escape it always did.
        assert_eq!(jni_short_name("a/Bé", "m"), "Java_a_B_000e9_m");
    }

    #[test]
    fn register_jni_native_overwrites() {
        extern "C" fn v1(_env: JNIEnv, _this: JObject) -> u64 {
            1
        }
        extern "C" fn v2(_env: JNIEnv, _this: JObject) -> u64 {
            2
        }
        let table = JniNativeMethodTable::default();
        register_jni_native_in(
            &table,
            "com/example/Baz",
            "quux",
            "()I",
            v1 as *const () as usize,
        );
        register_jni_native_in(
            &table,
            "com/example/Baz",
            "quux",
            "()I",
            v2 as *const () as usize,
        );
        assert_eq!(
            find_jni_native_in(&table, "com/example/Baz", "quux", "()I"),
            Some(v2 as *const () as usize)
        );
    }

    #[test]
    fn dispatch_jni_native_no_args() {
        // Verify dispatch_jni_native calls the function and returns the result.
        extern "C" fn always_99(_env: JNIEnv, _this: JObject) -> u64 {
            99
        }
        let fn_ptr = always_99 as *const () as usize;
        let env = get_jni_env();
        let result = unsafe { dispatch_jni_native(fn_ptr, env, 0, &[], "(I)I") };
        // return type is 'I', raw_result = 99 → Value::Int(99)
        assert_eq!(result, crate::types::Value::Int(99));
    }

    #[test]
    fn dispatch_jni_native_boolean_return_is_normalised_by_value() {
        // `0x80` has no bit 0 set, so the old `& 1` reduction called it false.
        // HotSpot's native wrapper tests the whole byte and calls it true.
        extern "C" fn flag_word(_env: JNIEnv, _this: JObject) -> u64 {
            0x80
        }
        extern "C" fn high_bits_only(_env: JNIEnv, _this: JObject) -> u64 {
            // Bits above the byte must NOT make it true: `jboolean` is a byte.
            0xff00
        }
        let env = get_jni_env();
        let r = unsafe { dispatch_jni_native(flag_word as *const () as usize, env, 0, &[], "()Z") };
        assert_eq!(r, crate::types::Value::Int(1), "0x80 is a true jboolean");
        let r = unsafe {
            dispatch_jni_native(high_bits_only as *const () as usize, env, 0, &[], "()Z")
        };
        assert_eq!(r, crate::types::Value::Int(0), "only the low byte counts");
    }

    #[test]
    fn dispatch_jni_native_void_return() {
        extern "C" fn do_nothing(_env: JNIEnv, _this: JObject) {}
        let fn_ptr = do_nothing as *const () as usize;
        let env = get_jni_env();
        let result = unsafe { dispatch_jni_native(fn_ptr, env, 0, &[], "()V") };
        assert_eq!(result, crate::types::Value::Object(None));
    }

    #[test]
    fn jni_register_natives_in_function_table() {
        // Index 215 must be RegisterNatives (not the stub).
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(215) };
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(
            func_ptr, stub_ptr,
            "index 215 should be RegisterNatives, not stub"
        );
    }

    #[test]
    fn jni_unregister_natives_in_function_table() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(216) };
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(
            func_ptr, stub_ptr,
            "index 216 should be UnregisterNatives, not stub"
        );
    }

    #[test]
    fn jni_bare_varargs_slots_are_dispatched_or_refused_loudly() {
        // The bare C-varargs `...` call slots cannot be WRITTEN in stable Rust
        // (defining a C-variadic function is still unstable), which is why
        // they used to be wired to `jni_varargs_unsupported`. They can be
        // ENTERED in assembly, though, and on x86-64 they now are: each slot
        // gets a trampoline that builds the platform `va_list` and hands off
        // to the `V` sibling. `jni_varargs_slots` is the single list of which
        // slot gets which, so asserting against it is asserting against the
        // wiring the table actually performs.
        //
        // What must hold on EVERY architecture is that none of these slots is
        // the silent `jni_stub`: a 0/null return is indistinguishable from a
        // real result to the native that receives it.
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        let refuse_ptr = jni_varargs_unsupported as *const () as usize;
        let expected = jni_varargs_slots();
        assert_eq!(expected.len(), 31, "NewObject plus three groups of ten");
        for (slot, want) in expected {
            let func_ptr = unsafe { *(*env).add(slot) };
            assert_ne!(
                func_ptr, stub_ptr,
                "bare-varargs slot {slot} must never be the silent stub"
            );
            assert_eq!(func_ptr, want, "bare-varargs slot {slot}");
            #[cfg(target_arch = "x86_64")]
            assert_ne!(
                func_ptr, refuse_ptr,
                "bare-varargs slot {slot} has a trampoline on x86-64"
            );
            #[cfg(not(target_arch = "x86_64"))]
            assert_eq!(
                func_ptr, refuse_ptr,
                "without a trampoline, slot {slot} must refuse loudly"
            );
        }
        let _ = refuse_ptr;
    }

    #[test]
    fn jni_va_list_and_jvalue_array_call_slots_are_wired() {
        // The V (va_list) and A (jvalue[]) call forms ARE implemented; their
        // slots must point at real functions, not the stub.
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        let varargs_ptr = jni_varargs_unsupported as *const () as usize;
        // V/A slots for instance / nonvirtual / static Object-returning calls
        // plus NewObjectV / NewObjectA.
        let va_list_and_array_slots = [
            29, 30, // NewObjectV / NewObjectA
            35, 36, // CallObjectMethodV / CallObjectMethodA
            65, 66, // CallNonvirtualObjectMethodV / ...A
            115, 116, // CallStaticObjectMethodV / ...A
        ];
        for slot in va_list_and_array_slots {
            let func_ptr = unsafe { *(*env).add(slot) };
            assert_ne!(func_ptr, stub_ptr, "V/A slot {slot} must be implemented");
            assert_ne!(
                func_ptr, varargs_ptr,
                "V/A slot {slot} must not be the varargs-unsupported stub"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M3: Global/Local reference management tests
    // -----------------------------------------------------------------------

    #[test]
    fn global_ref_handle_is_tagged() {
        // Global ref handles must have bit 0 set so jobject_to_obj can distinguish
        // them from local refs (raw heap pointers, always aligned → bit 0 = 0).
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        assert_eq!(handle & 1, 1, "global ref handle bit 0 must be set");
    }

    #[test]
    fn global_ref_jobject_to_obj_roundtrip() {
        // jobject_to_obj must correctly dereference a global ref handle.
        // NEW-11: the test previously relied on an unrelated earlier test
        // having set the JNI context TLS. In the non-synthetic default
        // build the inline `mod tests` in vm.rs is gated out, so fewer
        // tests run before this one and the TLS may be empty. The fix
        // is to set the context *and* add the ref via the shared VM's
        // global-refs table so the resolution path matches the lookup.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let handle = shared.natives.jni_global_refs.lock().add(obj);
        let resolved = jobject_to_obj(handle).expect("global ref must resolve to Some");
        assert_eq!(
            resolved, obj,
            "resolved global ref must match the original object"
        );
    }

    #[test]
    fn global_ref_gc_roots_included() {
        // collect_roots must include objects held by global refs.
        use crate::config::VmConfig;
        use crate::memory::roots::collect_roots;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        // Create a global ref via the shared state.
        shared.natives.jni_global_refs.lock().add(obj);
        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(
            roots.contains(&obj),
            "object held by global ref must appear in GC roots"
        );
    }

    #[test]
    fn global_ref_update_after_gc() {
        // update_after_gc must update the stored ObjectRef if the object moves.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        // Simulate GC moving the object to a new address.
        let old_addr = obj.as_ptr() as usize;
        let fake_new_addr = old_addr.wrapping_add(0x100); // pretend GC moved it
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr, fake_new_addr);
        refs.update_after_gc(&pointer_map);
        // The handle must now resolve to the new address.
        let updated = refs.resolve(handle).expect("handle must still be valid");
        assert_eq!(
            updated.as_ptr() as usize,
            fake_new_addr,
            "global ref must track GC movement"
        );
    }

    #[test]
    fn local_frame_push_pop() {
        // PushLocalFrame / PopLocalFrame must work correctly via the TLS stack.
        push_local_frame(8);
        track_local_ref(0x100); // fake local ref
        let result = pop_local_frame(0x200); // promote a different JObject
        assert_eq!(
            result, 0x200,
            "PopLocalFrame must return the provided result"
        );
    }

    /// gc-common w4-g: `PopLocalFrame`'s result is recorded in the parent
    /// frame (it used to be rooted by nothing after the pop), and
    /// `DeleteLocalRef` removes ONE of two equal local refs (it removed both,
    /// unrooting the one the native still held).
    #[test]
    fn pop_promotes_the_result_and_delete_removes_one_equal_ref() {
        // The recorded handles (what `collect_local_ref_roots` roots), read
        // without minting `ObjectRef`s for these fake addresses.
        let recorded = || {
            JNI_LOCAL_FRAMES.with(|f| {
                f.borrow()
                    .iter()
                    .flatten()
                    .copied()
                    .filter(|&h| h != 0 && h & 1 == 0)
                    .collect::<Vec<JObject>>()
            })
        };
        // Anything a frame below ours holds (none on a fresh test thread).
        let base = recorded().len();
        let roots_now = || recorded()[base..].to_vec();
        push_local_frame(4); // the native's implicit frame
        push_local_frame(4); // its PushLocalFrame scope
        record_local_handle(0x1000); // a temporary
        record_local_handle(0x2000); // the result it keeps
        assert_eq!(pop_local_frame(0x2000), 0x2000);
        assert_eq!(
            roots_now(),
            vec![0x2000],
            "the promoted result lives in the parent frame; the temporary is gone"
        );
        // A global ref / `jclass` result is not a local and is not recorded.
        push_local_frame(4);
        let _ = pop_local_frame(0x3001);
        assert_eq!(roots_now(), vec![0x2000]);

        // Two local refs to one object: deleting one keeps the other.
        record_local_handle(0x2000);
        delete_local_ref(0x2000);
        assert_eq!(roots_now(), vec![0x2000], "one equal ref must remain");
        delete_local_ref(0x2000);
        assert!(roots_now().is_empty());
        let _ = pop_local_frame(0);
    }

    /// gc-common w3-g: a local ref handed to native code is a GC root (and so
    /// remapped) while a local frame is open, and is NOT recorded on a thread
    /// with no open frame (a foreign-attached caller outside any native method),
    /// where a synthesized frame would never be popped.
    #[test]
    fn new_local_handle_is_rooted_only_inside_an_open_frame() {
        let (_shared, obj) = alloc_test_obj();
        let depth = JNI_LOCAL_FRAMES.with(|f| f.borrow().len());
        let roots_now = || {
            let mut v = Vec::new();
            collect_local_ref_roots(&mut v);
            v
        };
        if depth == 0 {
            let h = new_local_handle(obj);
            assert_eq!(h, obj_to_jobject(obj), "the handle is still the raw address");
            assert!(roots_now().is_empty(), "no open frame: nothing recorded");
            assert_eq!(JNI_LOCAL_FRAMES.with(|f| f.borrow().len()), 0, "no frame synthesized");
        }
        push_local_frame(4);
        let before = roots_now().len();
        let h = new_local_handle(obj);
        let slot = recorded_local_slot_of(h).expect("recorded in the open frame");
        let after = roots_now();
        assert_eq!(after.len(), before + 1);
        assert!(after.contains(&obj), "the handed-out local ref must be a root");
        // Remapped with the frame, like every recorded local ref -- and the
        // slot reads back the CURRENT address (what `NewObject` now returns
        // after an `<init>` that let a moving collection run).
        let moved = obj.as_ptr() as usize + 0x1000;
        update_local_refs_after_gc(&cratonvm_types::PointerMap::from_iter([(
            h as usize, moved,
        )]));
        assert!(roots_now().iter().any(|r| r.as_ptr() as usize == moved));
        assert_eq!(recorded_local_at(slot), Some(moved as JObject));
        // A frame left open above it makes the slot unreadable, not wrong.
        push_local_frame(4);
        assert_eq!(recorded_local_at(slot), None);
        let _ = pop_local_frame(0);
        // A global-ref / `jclass` handle is never recorded as a local.
        record_local_handle(h | 1);
        assert_eq!(roots_now().len(), before + 1);
        let _ = pop_local_frame(0);
        assert_eq!(JNI_LOCAL_FRAMES.with(|f| f.borrow().len()), depth);
    }

    #[test]
    fn delete_local_ref_removes_from_frame() {
        push_local_frame(4);
        track_local_ref(0x1000);
        track_local_ref(0x2000);
        delete_local_ref(0x1000);
        // After deletion, only 0x2000 remains (verified by popping the frame).
        JNI_LOCAL_FRAMES.with(|f| {
            let stack = f.borrow();
            let top = stack.last().expect("frame must exist");
            assert!(!top.contains(&0x1000), "0x1000 must be deleted");
            assert!(top.contains(&0x2000), "0x2000 must remain");
        });
        let _ = pop_local_frame(0);
    }

    /// Switches `CRATONVM_JNI_INDIRECT_LOCALS` on for the calling test thread
    /// until dropped.
    struct IndirectLocalsOn;

    impl IndirectLocalsOn {
        fn new() -> Self {
            INDIRECT_LOCALS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            IndirectLocalsOn
        }
    }

    impl Drop for IndirectLocalsOn {
        fn drop(&mut self) {
            INDIRECT_LOCALS_TEST_OVERRIDE.with(|c| c.set(None));
        }
    }

    /// gc-common w6-g: the flag's value rule and the handle encoding.
    #[test]
    fn indirect_local_flag_rule_and_encoding() {
        assert!(!indirect_locals_from(None));
        for off in ["", "0", "false", "off", "no", " 0 "] {
            assert!(!indirect_locals_from(Some(off)), "{off:?} is off");
        }
        for on in ["1", "true", "on", "yes"] {
            assert!(indirect_locals_from(Some(on)), "{on:?} is on");
        }
        assert_eq!(
            indirect_locals_test_override(),
            None,
            "tests default to the latched flag"
        );

        let h = encode_indirect_local(3, 7).expect("encodable");
        assert_eq!(h & 1, 0, "never reads as a global ref");
        assert!(!is_jclass_handle(h), "never reads as a jclass");
        assert!(is_indirect_local_tag(h));
        assert!(!is_indirect_local_tag(class_id_to_jclass(ClassId::new(9))));
        assert!(
            !is_indirect_local_tag(0x0000_7FFF_FFFF_FFF8),
            "a user-space address"
        );
        assert_eq!(encode_indirect_local(INDIRECT_LOCAL_MAX_DEPTH + 1, 0), None);
        assert_eq!(encode_indirect_local(0, INDIRECT_LOCAL_MAX_SLOT + 1), None);
        let edge = encode_indirect_local(INDIRECT_LOCAL_MAX_DEPTH, INDIRECT_LOCAL_MAX_SLOT)
            .expect("the largest pair fits");

        // Decoding is gated on the flag: with it off a tagged value is just a
        // bad address (the pre-w6 answer).
        INDIRECT_LOCALS_TEST_OVERRIDE.with(|c| c.set(Some(false)));
        assert_eq!(decode_indirect_local(h), None);
        let _on = IndirectLocalsOn::new();
        assert_eq!(decode_indirect_local(h), Some((3, 7)));
        assert_eq!(
            decode_indirect_local(edge),
            Some((INDIRECT_LOCAL_MAX_DEPTH, INDIRECT_LOCAL_MAX_SLOT))
        );
        assert_eq!(
            decode_indirect_local(h | 1),
            None,
            "a global ref is not a local slot"
        );
    }

    /// gc-common w6-g (`common-w2c-jni-local-refs-are-raw-addresses`): with
    /// the indirection on, the NATIVE'S OWN copy of a local ref follows a
    /// moving collection, because it names the remapped slot, not the address.
    /// `NewLocalRef`, `IsSameObject`, `DeleteLocalRef` (slot cleared, trailing
    /// slot reused, no index shift) and `PopLocalFrame` (result re-created in
    /// the parent) all work on the indirect form, and a raw handle still
    /// resolves.
    #[test]
    fn indirect_local_handles_follow_a_moving_collection() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let a = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        // Stands in for `a`'s to-space copy.
        let b = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        let on = IndirectLocalsOn::new();
        let depth = JNI_LOCAL_FRAMES.with(|f| f.borrow().len());
        if depth == 0 {
            assert_eq!(
                new_local_handle(a),
                obj_to_jobject(a),
                "no open frame: raw, as before"
            );
        }
        push_local_frame(4);
        let h = new_local_handle(a);
        assert!(is_indirect_local_tag(h));
        assert_eq!(jobject_to_obj(h), Some(a));

        // The collection moved `a` to `b`: the table is rewritten, and the
        // native's unchanged `h` now names the new address.
        update_local_refs_after_gc(&cratonvm_types::PointerMap::from_iter([(
            a.as_ptr() as usize,
            b.as_ptr() as usize,
        )]));
        assert_eq!(jobject_to_obj(h), Some(b));

        let h2 = jni_new_local_ref(get_jni_env(), h);
        assert!(is_indirect_local_tag(h2));
        assert_ne!(h2, h);
        assert_eq!(jni_is_same_object(get_jni_env(), h, h2), JNI_TRUE);
        assert_eq!(jni_get_object_ref_type(get_jni_env(), h2), 1, "a local ref");

        jni_delete_local_ref(get_jni_env(), h2);
        assert_eq!(jobject_to_obj(h2), None, "a deleted local is NULL");
        assert_eq!(jobject_to_obj(h), Some(b), "the other local is untouched");
        assert_eq!(
            new_local_handle(a),
            h2,
            "the trailing deleted slot is reused"
        );

        // A raw local in the same frame: deleting it clears, never shifts.
        let raw_before = JNI_LOCAL_FRAMES.with(|f| f.borrow()[depth].len());
        record_local_handle(obj_to_jobject(a));
        let after_raw = new_local_handle(b);
        delete_local_ref(obj_to_jobject(a));
        assert_eq!(jobject_to_obj(after_raw), Some(b), "no later slot moved");
        assert_eq!(
            JNI_LOCAL_FRAMES.with(|f| f.borrow()[depth].len()),
            raw_before + 2
        );
        assert_eq!(
            jobject_to_obj(obj_to_jobject(b)),
            Some(b),
            "raw handles still resolve"
        );

        // PopLocalFrame: the result is read before the pop and re-created in
        // the parent frame; the popped frame's other locals are gone.
        push_local_frame(4);
        let keep = new_local_handle(a);
        let temp = new_local_handle(b);
        let promoted = pop_local_frame(keep);
        assert!(is_indirect_local_tag(promoted));
        assert_eq!(decode_indirect_local(promoted).map(|(d, _)| d), Some(depth));
        assert_eq!(jobject_to_obj(promoted), Some(a));
        assert_eq!(jobject_to_obj(temp), None, "its frame was popped");

        let _ = pop_local_frame(0);
        assert_eq!(JNI_LOCAL_FRAMES.with(|f| f.borrow().len()), depth);
        drop(on);
        // Off again: handles are raw addresses, byte for byte.
        push_local_frame(4);
        assert_eq!(new_local_handle(a), obj_to_jobject(a));
        let _ = pop_local_frame(0);
    }

    /// The text of a `jstring`, read through `GetStringUTFChars`.
    fn jstring_text(env: JNIEnv, s: JObject) -> Option<String> {
        let p = jni_get_string_utf_chars(env, s, std::ptr::null_mut());
        if p.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(p) }.to_str().ok().map(str::to_owned);
        jni_release_string_utf_chars(env, s, p);
        text
    }

    /// gc-common w7-c (`common-w2c-jni-local-refs-are-raw-addresses`): with
    /// `CRATONVM_JNI_INDIRECT_LOCALS` on, every local-ref producer the JNI
    /// table has that needs no loaded class hands out an indirect handle, and
    /// every one of them still names the right, intact object after a REAL
    /// collection of `gc_algorithm`'s heap. The collection is driven exactly
    /// as a pause drives it for the JNI tables: roots from
    /// `collect_local_ref_roots` + the global table (`roots.rs` 9 / 9b), the
    /// collector's pointer map applied by `update_local_refs_after_gc` and
    /// `JniGlobalRefs::update_after_gc` (`gc.rs::update_all_roots`). Whether
    /// this backend moved an object in that cycle depends on its policy, so a
    /// forced relocation onto a genuine copy follows, on the same heap, to
    /// exercise the moved path every time.
    ///
    /// Covered: NewStringUTF, NewString, New<Type>Array, NewObjectArray,
    /// Get/SetObjectArrayElement, NewLocalRef, DeleteLocalRef (slot reuse),
    /// EnsureLocalCapacity, PushLocalFrame / PopLocalFrame (promoted result,
    /// popped temporary), Throw / ExceptionOccurred / ExceptionClear,
    /// NewGlobalRef / NewWeakGlobalRef from an indirect local,
    /// GetObjectClass, IsInstanceOf, IsSameObject, GetObjectRefType,
    /// Get/SetIntArrayRegion, GetArrayLength, GetStringUTFChars,
    /// GetStringLength, MonitorEnter / MonitorExit across the collection.
    /// `NewObject*`, field access and method calls need loaded classes and
    /// are `indirect_locals_through_constructors_fields_and_calls`.
    fn indirect_locals_follow_a_real_collection_on(gc_algorithm: crate::config::GcAlgorithm) {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::SharedVm;
        let addr = |o: ObjectRef| o.as_ptr() as usize;
        let shared = Arc::new(SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        }));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "jni-indirect", None);
        assert_ne!(worker, ThreadId(0));
        let mut thread = JvmThread::new(worker, "jni-indirect");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let _ = take_jni_pending_exception();
        let env = get_jni_env();
        let on = IndirectLocalsOn::new();
        let base = local_frame_depth();
        push_local_frame(16); // the native's implicit dispatch frame

        // ---- producers ----
        let text = CString::new("w7-c indirect local").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        let units: Vec<u16> = "w7-c utf16".encode_utf16().collect();
        let s16 = jni_new_string(env, units.as_ptr(), units.len() as JSize);
        let ints = jni_new_int_array(env, 4);
        jni_set_int_array_region(env, ints, 0, 4, [1, 2, 3, 4].as_ptr());
        let string_class = jni_get_object_class(env, s);
        assert_ne!(string_class, 0);
        assert_eq!(jni_is_instance_of(env, s, string_class), JNI_TRUE);
        let objs = jni_new_object_array(env, 2, string_class, 0);
        jni_set_object_array_element(env, objs, 0, s16);
        let elem = jni_get_object_array_element(env, objs, 0);
        let copy = jni_new_local_ref(env, s);
        assert_eq!(jni_ensure_local_capacity(env, 64), JNI_OK);
        // A PushLocalFrame scope: one temporary, one promoted result.
        assert_eq!(jni_push_local_frame(env, 4), JNI_OK);
        let scoped_tmp = jni_new_int_array(env, 1);
        let scoped_keep = jni_new_int_array(env, 3);
        jni_set_int_array_region(env, scoped_keep, 0, 3, [7, 8, 9].as_ptr());
        let kept = jni_pop_local_frame(env, scoped_keep);
        // New* + DeleteLocalRef reuses the trailing slot.
        let doomed = jni_new_int_array(env, 1);
        jni_delete_local_ref(env, doomed);
        let reused = jni_new_int_array(env, 2);
        assert_eq!(reused, doomed, "the deleted trailing slot is reused");
        // The t = ExceptionOccurred(); ExceptionClear(); ... use t idiom. Any
        // object serves: `Throw` does not check the type.
        let thrown_obj = jni_new_int_array(env, 5);
        assert_eq!(jni_throw(env, thrown_obj), JNI_OK);
        let thrown = jni_exception_occurred(env);
        jni_exception_clear(env);
        assert_eq!(jni_is_same_object(env, thrown, thrown_obj), JNI_TRUE);
        // Global refs minted FROM indirect locals.
        let global = jni_new_global_ref(env, ints);
        let weak = jni_new_weak_global_ref(env, objs);
        assert_eq!(global & 1, 1);
        assert_eq!(weak & 1, 1);
        assert_eq!(jni_monitor_enter(env, objs), JNI_OK);

        let locals = [s, s16, ints, objs, elem, copy, kept, reused, thrown];
        for &h in &locals {
            assert!(is_indirect_local_tag(h), "{h:#x} must be an indirect handle");
            assert_eq!(jni_get_object_ref_type(env, h), 1, "a local ref");
        }
        assert_eq!(jobject_to_obj(scoped_tmp), None, "its frame was popped");
        assert_eq!(jni_get_object_ref_type(env, global), 2);
        let before: Vec<usize> = locals
            .iter()
            .map(|&h| jobject_to_obj(h).map(addr).expect("resolves before the collection"))
            .collect();
        assert!(shared.threads.monitors.holds(jobject_to_obj(objs).unwrap(), worker));

        // ---- a real collection, rooted and remapped as a pause does ----
        let mut roots = Vec::new();
        collect_local_ref_roots(&mut roots);
        shared.natives.jni_global_refs.lock().collect_roots(&mut roots);
        // gc-common w8-c: the weak global is swept, not remapped by
        // `update_after_gc`; watched first, as the pause does.
        cratonvm_gc::gc_quiescence::set_watched_referents(&[]);
        watch_weak_global_referents(&shared);
        // SAFETY: this test's SharedVm has no mutator but this thread.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let result = shared
            .mem
            .heap
            .collect_garbage(&stw, &mut roots, &shared.threads.monitors);
        update_local_refs_after_gc(&result.pointer_map);
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(&result.pointer_map);
        let _ = sweep_weak_global_refs(&shared, &result.pointer_map);
        let mut moved = 0usize;
        for (&h, &old) in locals.iter().zip(&before) {
            let expected = match result.pointer_map.get(&old) {
                Some(&to) => {
                    moved += 1;
                    to
                }
                None => old,
            };
            assert_eq!(
                jobject_to_obj(h).map(addr),
                Some(expected),
                "{gc_algorithm:?}: the native's unchanged handle names the object's \
                 post-collection address"
            );
        }
        eprintln!("{gc_algorithm:?}: {moved} of {} locals moved", locals.len());

        // ---- consumers, through the same handles ----
        assert_eq!(jstring_text(env, s).as_deref(), Some("w7-c indirect local"));
        assert_eq!(jni_get_string_length(env, s16), units.len() as JSize);
        let mut out = [0 as JInt; 4];
        jni_get_int_array_region(env, ints, 0, 4, out.as_mut_ptr());
        assert_eq!(out, [1, 2, 3, 4]);
        let mut kept_out = [0 as JInt; 3];
        jni_get_int_array_region(env, kept, 0, 3, kept_out.as_mut_ptr());
        assert_eq!(kept_out, [7, 8, 9], "the promoted PopLocalFrame result");
        assert_eq!(jni_get_array_length(env, objs), 2);
        assert_eq!(jni_get_array_length(env, reused), 2);
        assert_eq!(jni_get_array_length(env, thrown), 5);
        assert_eq!(jni_is_same_object(env, elem, s16), JNI_TRUE);
        let elem_again = jni_get_object_array_element(env, objs, 0);
        assert_eq!(jni_is_same_object(env, elem_again, s16), JNI_TRUE);
        assert_eq!(jni_is_same_object(env, copy, s), JNI_TRUE);
        assert_eq!(jni_is_same_object(env, global, ints), JNI_TRUE);
        assert_eq!(jni_is_same_object(env, weak, objs), JNI_TRUE);
        assert_eq!(jni_is_same_object(env, s, s16), JNI_FALSE);
        assert!(
            shared.threads.monitors.holds(jobject_to_obj(objs).unwrap(), worker),
            "{gc_algorithm:?}: the monitor follows the object"
        );
        assert_eq!(jni_monitor_exit(env, objs), JNI_OK);
        assert!(!shared.threads.monitors.holds(jobject_to_obj(objs).unwrap(), worker));

        // ---- a forced relocation onto a genuine copy, on this heap ----
        let fresh = jni_new_int_array(env, 4);
        jni_set_int_array_region(env, fresh, 0, 4, [1, 2, 3, 4].as_ptr());
        let from = jobject_to_obj(ints).map(addr).unwrap();
        let to = jobject_to_obj(fresh).map(addr).unwrap();
        assert_ne!(from, to);
        let map = cratonvm_types::PointerMap::from_iter([(from, to)]);
        update_local_refs_after_gc(&map);
        shared.natives.jni_global_refs.lock().update_after_gc(&map);
        assert_eq!(jobject_to_obj(ints).map(addr), Some(to));
        assert_eq!(jni_is_same_object(env, ints, fresh), JNI_TRUE);
        assert_eq!(jni_is_same_object(env, global, fresh), JNI_TRUE);
        let mut out = [0 as JInt; 4];
        jni_get_int_array_region(env, ints, 0, 4, out.as_mut_ptr());
        assert_eq!(out, [1, 2, 3, 4]);

        jni_delete_global_ref(env, global);
        jni_delete_weak_global_ref(env, weak);
        truncate_local_frames(base);
        assert_eq!(local_frame_depth(), base);
        assert_eq!(jobject_to_obj(s), None, "a popped frame's locals are NULL");
        drop(on);
    }

    #[test]
    fn indirect_locals_follow_a_real_generational_collection() {
        indirect_locals_follow_a_real_collection_on(crate::config::GcAlgorithm::Generational);
    }

    #[test]
    fn indirect_locals_follow_a_real_g1_collection() {
        indirect_locals_follow_a_real_collection_on(crate::config::GcAlgorithm::G1);
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn indirect_locals_follow_a_real_zgc_collection() {
        indirect_locals_follow_a_real_collection_on(crate::config::GcAlgorithm::Zgc);
    }

    /// gc-common w9-g (`common-w2c-jni-local-refs-are-raw-addresses`): the
    /// copy-out / copy-back JNI functions with INDIRECT handles, across a REAL
    /// collection of `gc_algorithm`'s heap and then a forced relocation onto
    /// genuine copies: `Get/Release<Int>ArrayElements`,
    /// `Get/ReleasePrimitiveArrayCritical` (a `byte[]`, and a second critical
    /// open at once on an EMPTY array), `GetStringChars` / `GetStringCritical`
    /// and their releases, `GetStringRegion` / `GetStringUTFRegion`. Each
    /// copy-back lands in the array the native's unchanged handle names NOW,
    /// and every keep-alive global ref the Gets minted is gone after the final
    /// release. Needs no JDK class.
    fn indirect_locals_array_copies_follow_a_real_collection_on(
        gc_algorithm: crate::config::GcAlgorithm,
    ) {
        use crate::config::VmConfig;
        let addr = |o: ObjectRef| o.as_ptr() as usize;
        let shared = Arc::new(SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        }));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "jni-copies", None);
        let mut thread = JvmThread::new(worker, "jni-copies");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let env = get_jni_env();
        let _on = IndirectLocalsOn::new();
        push_local_frame(16); // the native's implicit dispatch frame
        let globals = || shared.natives.jni_global_refs.lock().count();
        let globals_before = globals();

        let ints = jni_new_int_array(env, 4);
        jni_set_int_array_region(env, ints, 0, 4, [1, 2, 3, 4].as_ptr());
        let bytes = jni_new_byte_array(env, 3);
        jni_set_byte_array_region(env, bytes, 0, 3, [1 as JByte, 2, 3].as_ptr());
        let empty = jni_new_int_array(env, 0);
        let text = CString::new("w9-g").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        for h in [ints, bytes, empty, s] {
            assert!(is_indirect_local_tag(h), "{h:#x} must be an indirect handle");
        }
        let utf16: Vec<u16> = "w9-g".encode_utf16().collect();

        // ---- the Gets, all before the collection ----
        let mut is_copy: JBoolean = JNI_FALSE;
        let elems = jni_get_int_array_elements(env, ints, &mut is_copy);
        assert!(!elems.is_null());
        assert_eq!(is_copy, JNI_TRUE, "always a detached copy");
        assert_eq!(unsafe { std::slice::from_raw_parts(elems, 4) }, &[1, 2, 3, 4]);
        let crit = jni_get_primitive_array_critical(env, bytes, std::ptr::null_mut());
        assert!(!crit.is_null());
        let crit_empty = jni_get_primitive_array_critical(env, empty, std::ptr::null_mut());
        assert!(!crit_empty.is_null(), "an empty array still gets its own buffer");
        assert_ne!(crit_empty, crit);
        assert_eq!(globals(), globals_before + 3, "one keep-alive global per open copy");
        let chars = jni_get_string_chars(env, s, std::ptr::null_mut());
        let crit_chars = jni_get_string_critical(env, s, std::ptr::null_mut());
        for p in [chars, crit_chars] {
            assert!(!p.is_null());
            assert_eq!(unsafe { std::slice::from_raw_parts(p, utf16.len()) }, &utf16[..]);
        }
        let before: Vec<usize> = [ints, bytes, s]
            .iter()
            .map(|&h| jobject_to_obj(h).map(addr).expect("resolves before the collection"))
            .collect();

        // ---- a real collection, rooted and remapped as a pause does ----
        let (map, _) = weak_sweep_collection(&shared);
        let moved = before
            .iter()
            .filter(|&&a| map.get(&a).is_some_and(|&to| to != a))
            .count();
        eprintln!("{gc_algorithm:?}: {moved} of {} arrays/strings moved", before.len());

        // The native writes into its copies after the collection ran...
        let crit_bytes = crit as *mut JByte;
        unsafe {
            for (i, v) in [10, 20, 30, 40].into_iter().enumerate() {
                *elems.add(i) = v;
            }
            for (i, v) in [7 as JByte, 8, 9].into_iter().enumerate() {
                *crit_bytes.add(i) = v;
            }
        }
        // ...and a forced relocation onto genuine copies follows, on this heap.
        let ints_to = jni_new_int_array(env, 4);
        let bytes_to = jni_new_byte_array(env, 3);
        let from_i = jobject_to_obj(ints).map(addr).unwrap();
        let from_b = jobject_to_obj(bytes).map(addr).unwrap();
        let to_i = jobject_to_obj(ints_to).map(addr).unwrap();
        let to_b = jobject_to_obj(bytes_to).map(addr).unwrap();
        assert!(from_i != to_i && from_b != to_b);
        let reloc = cratonvm_types::PointerMap::from_iter([(from_i, to_i), (from_b, to_b)]);
        update_local_refs_after_gc(&reloc);
        shared.natives.jni_global_refs.lock().update_after_gc(&reloc);
        assert_eq!(jobject_to_obj(ints).map(addr), Some(to_i));
        assert_eq!(jobject_to_obj(bytes).map(addr), Some(to_b));

        // ---- the Releases write back to where the arrays are NOW ----
        jni_release_int_array_elements(env, ints, elems, 0);
        jni_release_primitive_array_critical(env, empty, crit_empty, 0);
        jni_release_primitive_array_critical(env, bytes, crit, 0);
        jni_release_string_critical(env, s, crit_chars);
        jni_release_string_chars(env, s, chars);
        let mut out = [0 as JInt; 4];
        jni_get_int_array_region(env, ints, 0, 4, out.as_mut_ptr());
        assert_eq!(
            out,
            [10, 20, 30, 40],
            "{gc_algorithm:?}: Release<Int>ArrayElements writes into the relocated array"
        );
        let mut bout = [0 as JByte; 3];
        jni_get_byte_array_region(env, bytes, 0, 3, bout.as_mut_ptr());
        assert_eq!(
            bout,
            [7, 8, 9],
            "{gc_algorithm:?}: ReleasePrimitiveArrayCritical writes into the relocated array"
        );
        assert_eq!(globals(), globals_before, "every keep-alive global ref was deleted");
        assert_eq!(
            shared.natives.jni_global_refs.lock().outstanding_copies(),
            0,
            "the element and string copy records went with their releases"
        );
        assert!(JNI_CRITICAL_COPIES.with(|c| {
            let c = c.borrow();
            !c.contains_key(&(crit as usize)) && !c.contains_key(&(crit_empty as usize))
        }));

        // The string regions read through the same indirect handle.
        let mut region = [0u16; 2];
        jni_get_string_region(env, s, 1, 2, region.as_mut_ptr());
        assert_eq!(region, [utf16[1], utf16[2]]);
        let mut utf = [0 as c_char; 2];
        jni_get_string_utf_region(env, s, 0, 2, utf.as_mut_ptr());
        assert_eq!([utf[0] as u8, utf[1] as u8], *b"w9");
        assert_eq!(take_jni_pending_exception(), None, "no call raised");
    }

    #[test]
    fn indirect_locals_array_copies_follow_a_real_generational_collection() {
        indirect_locals_array_copies_follow_a_real_collection_on(
            crate::config::GcAlgorithm::Generational,
        );
    }

    #[test]
    fn indirect_locals_array_copies_follow_a_real_g1_collection() {
        indirect_locals_array_copies_follow_a_real_collection_on(crate::config::GcAlgorithm::G1);
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn indirect_locals_array_copies_follow_a_real_zgc_collection() {
        indirect_locals_array_copies_follow_a_real_collection_on(crate::config::GcAlgorithm::Zgc);
    }

    /// gc-common w9-g (`common-w2c-jni-local-refs-are-raw-addresses`, the
    /// "stays raw" cases the page lists): with the flag ON, a local handed
    /// out on a FOREIGN-ATTACHED thread, or in a frame deeper than the
    /// encoding can name, is still the raw address -- recorded (so rooted) in
    /// the open frame, and resolving exactly as with the flag off -- while the
    /// same call back in range hands out an indirect handle again.
    #[test]
    fn indirect_locals_stay_raw_where_a_slot_cannot_name_them() {
        use crate::config::VmConfig;
        /// Takes this test thread's fake attachment back out, on unwind too.
        struct FakeForeignAttach;
        impl Drop for FakeForeignAttach {
            fn drop(&mut self) {
                let _ = FOREIGN_THREAD_BOX.try_with(|c| c.borrow_mut().take());
            }
        }
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        let _jni = JniTls::context(&shared);
        let _on = IndirectLocalsOn::new();
        let base = local_frame_depth();

        // (b) A foreign-attached thread: its per-call frame closes at the end
        // of each JNI call while its natives keep locals across calls.
        {
            assert!(!is_foreign_attached(), "precondition: a VM-created test thread");
            let _attached = FakeForeignAttach;
            FOREIGN_THREAD_BOX.with(|c| {
                *c.borrow_mut() = Some(ForeignThreadBox::unregistered(Box::new(JvmThread::new(
                    ThreadId(0),
                    "w9g-foreign",
                ))));
            });
            assert!(is_foreign_attached());
            push_local_frame(4);
            let h = new_local_handle(obj);
            assert_eq!(h, obj_to_jobject(obj), "raw on a foreign-attached thread");
            assert!(!is_indirect_local_tag(h));
            assert_eq!(
                recorded_local_slot_of(h).map(|(depth, _)| depth),
                Some(base),
                "still recorded, so still a root, in the open frame"
            );
            assert_eq!(jobject_to_obj(h), Some(obj));
            truncate_local_frames(base);
        }
        assert!(!is_foreign_attached());

        // (c) A frame deeper than `INDIRECT_LOCAL_MAX_DEPTH`.
        while local_frame_depth() <= INDIRECT_LOCAL_MAX_DEPTH + 1 {
            push_local_frame(0);
        }
        let deep = new_local_handle(obj);
        assert_eq!(deep, obj_to_jobject(obj), "a depth no handle can encode stays raw");
        assert_eq!(jobject_to_obj(deep), Some(obj));
        truncate_local_frames(base);

        // Back in range: indirect again.
        push_local_frame(4);
        let near = new_local_handle(obj);
        assert!(is_indirect_local_tag(near));
        assert_eq!(jobject_to_obj(near), Some(obj));
        truncate_local_frames(base);
    }

    /// Roots as a pause gathers them for the JNI tables (`roots.rs` 9 / 9b),
    /// the watch publication, a REAL collection of the test heap, then the
    /// JNI half of the epilogue: locals and strong globals remapped
    /// (`update_all_roots`), weak globals swept (`sweep_weak_global_refs`).
    fn weak_sweep_collection(
        shared: &SharedVm,
    ) -> (cratonvm_types::PointerMap, WeakGlobalSweepStats) {
        let mut roots = Vec::new();
        collect_local_ref_roots(&mut roots);
        shared.natives.jni_global_refs.lock().collect_roots(&mut roots);
        cratonvm_gc::gc_quiescence::set_watched_referents(&[]);
        watch_weak_global_referents(shared);
        // SAFETY: the test's SharedVm has no mutator but this thread.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let result = shared
            .mem
            .heap
            .collect_garbage(&stw, &mut roots, &shared.threads.monitors);
        update_local_refs_after_gc(&result.pointer_map);
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(&result.pointer_map);
        let stats = sweep_weak_global_refs(shared, &result.pointer_map);
        (result.pointer_map, stats)
    }

    /// gc-common w8-c (`common-w7c-jni-weak-global-refs-are-strong`): on a
    /// REAL collection of `gc_algorithm`'s heap, a weak global is not a root;
    /// its referent, once nothing else reaches it, is collected and the weak
    /// global reads as NULL (`IsSameObject(w, NULL)`, `NewLocalRef(w) == NULL`),
    /// while a weak global to an object something else keeps alive names that
    /// object, relocated or not, with its contents intact. `GetObjectRefType`
    /// answers 3 for a weak global, before and after its referent dies.
    fn weak_globals_across_a_real_collection_on(gc_algorithm: crate::config::GcAlgorithm) {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::JvmThread;
        let addr = |o: ObjectRef| o.as_ptr() as usize;
        let shared = Arc::new(SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        }));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "jni-weak", None);
        let mut thread = JvmThread::new(worker, "jni-weak");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let _ = take_jni_pending_exception();
        let env = get_jni_env();
        let base = local_frame_depth();
        push_local_frame(16); // the native's implicit dispatch frame

        // `live` is held by a strong global (and its local); `doomed` only by
        // a weak global once its local is deleted.
        let strong_before = shared.natives.jni_global_refs.lock().count();
        let live = jni_new_int_array(env, 3);
        jni_set_int_array_region(env, live, 0, 3, [4, 5, 6].as_ptr());
        let strong = jni_new_global_ref(env, live);
        let w_live = jni_new_weak_global_ref(env, live);
        let doomed = jni_new_int_array(env, 2);
        let w_dead = jni_new_weak_global_ref(env, doomed);
        let dead_addr = jobject_to_obj(doomed).map(addr).expect("resolves");
        jni_delete_local_ref(env, doomed);
        assert_ne!(w_live, 0);
        assert_ne!(w_dead, 0);
        assert_eq!(jni_get_object_ref_type(env, strong), 2, "JNIGlobalRefType");
        assert_eq!(jni_get_object_ref_type(env, w_live), 3, "JNIWeakGlobalRefType");
        assert_eq!(jni_is_same_object(env, w_live, live), JNI_TRUE);
        assert_eq!(jni_is_same_object(env, w_dead, 0), JNI_FALSE, "not collected yet");

        // Not a root: the strong globals report `live` once, and never `doomed`.
        let mut table_roots = Vec::new();
        shared
            .natives
            .jni_global_refs
            .lock()
            .collect_roots(&mut table_roots);
        assert_eq!(
            table_roots.len(),
            strong_before + 1,
            "{gc_algorithm:?}: of the two globals made here, only the strong one is a root"
        );
        assert!(!table_roots.iter().any(|&r| addr(r) == dead_addr));

        let (map, stats) = weak_sweep_collection(&shared);
        eprintln!("{gc_algorithm:?}: first collection, weak sweep {stats:?}");
        assert!(
            !map.contains_key(&dead_addr),
            "{gc_algorithm:?}: an object reached only through a weak global was relocated \
             as a survivor: the weak global is still a root"
        );
        assert_eq!(
            jni_is_same_object(env, w_dead, 0),
            JNI_TRUE,
            "{gc_algorithm:?}: IsSameObject(wref, NULL) after its referent was collected"
        );
        assert_eq!(jni_new_local_ref(env, w_dead), 0, "NewLocalRef of a cleared weak");
        assert_eq!(jni_new_global_ref(env, w_dead), 0, "NewGlobalRef of a cleared weak");
        assert_eq!(jni_new_weak_global_ref(env, w_dead), 0);
        assert_eq!(
            jni_get_object_ref_type(env, w_dead),
            3,
            "a cleared weak global is still a valid weak handle until it is deleted"
        );
        assert!(stats.cleared >= 1, "{gc_algorithm:?}: {stats:?}");

        // The weak global to the strongly held object followed it.
        assert_eq!(jni_is_same_object(env, w_live, strong), JNI_TRUE);
        // Not `live`: with the default raw-address locals
        // (`common-w2c-jni-local-refs-are-raw-addresses`) that handle is the
        // pre-collection address, which a moving collection leaves behind.
        // A local made now from the (remapped) strong global is current.
        let live_now = jni_new_local_ref(env, strong);
        assert_eq!(jni_is_same_object(env, w_live, live_now), JNI_TRUE);
        let via_weak = jni_new_local_ref(env, w_live);
        assert_ne!(via_weak, 0);
        let mut out = [0 as JInt; 3];
        jni_get_int_array_region(env, via_weak, 0, 3, out.as_mut_ptr());
        assert_eq!(out, [4, 5, 6], "{gc_algorithm:?}: the weak referent's contents");

        // Drop every strong path to `live`. The survivor of the first
        // collection may have been tenured, and a young-only cycle does not
        // collect the old generation (or G1's old regions), so this is a
        // bounded retry, and a referent the backend's policy still retains
        // must at least still be a real object: the weak global never dangles.
        jni_delete_global_ref(env, strong);
        truncate_local_frames(base);
        push_local_frame(16);
        let mut cleared = false;
        for round in 0..3 {
            let (_, stats) = weak_sweep_collection(&shared);
            eprintln!(
                "{gc_algorithm:?}: collection {} after the drop, weak sweep {stats:?}",
                round + 2
            );
            if jni_is_same_object(env, w_live, 0) == JNI_TRUE {
                cleared = true;
                break;
            }
        }
        if cleared {
            assert_eq!(jni_new_local_ref(env, w_live), 0);
        } else {
            let still = shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(w_live)
                .map(addr)
                .expect("not cleared");
            assert!(
                shared.mem.heap.is_object_address(still).is_some(),
                "{gc_algorithm:?}: an uncleared weak global must name a real object"
            );
            eprintln!(
                "{gc_algorithm:?}: the dropped referent was retained by the backend's \
                 policy for 3 cycles (tenured); the clear-on-death half is the \
                 first collection's `w_dead` check"
            );
        }

        jni_delete_weak_global_ref(env, w_live);
        jni_delete_weak_global_ref(env, w_dead);
        {
            let refs = shared.natives.jni_global_refs.lock();
            assert_eq!(refs.weak_count(), 0, "DeleteWeakGlobalRef frees cleared handles too");
            assert_eq!(refs.kind(w_live), None);
        }
        truncate_local_frames(base);
    }

    #[test]
    fn weak_globals_across_a_real_generational_collection() {
        weak_globals_across_a_real_collection_on(crate::config::GcAlgorithm::Generational);
    }

    #[test]
    fn weak_globals_across_a_real_g1_collection() {
        weak_globals_across_a_real_collection_on(crate::config::GcAlgorithm::G1);
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn weak_globals_across_a_real_zgc_collection() {
        weak_globals_across_a_real_collection_on(crate::config::GcAlgorithm::Zgc);
    }

    /// gc-common w8-c: the table half of weak globals, with no collection.
    /// A weak entry is not reported by `collect_roots`, not rewritten by
    /// `update_after_gc` (the sweep owns it), resolves to NULL once cleared
    /// without leaving the table, and is removed by either delete.
    #[test]
    fn weak_entries_are_not_roots_and_are_not_remapped_by_update_after_gc() {
        let (_shared, obj) = alloc_test_obj();
        let a = obj.as_ptr() as usize;
        let mut refs = JniGlobalRefs::new();
        let s = refs.add(obj);
        let w = refs.add_weak(obj);
        assert_eq!(refs.kind(s), Some(JniGlobalKind::Strong));
        assert_eq!(refs.kind(w), Some(JniGlobalKind::Weak));
        assert!(refs.is_weak(w));
        assert!(!refs.is_weak(s));
        assert_eq!(refs.count(), 1, "count() is the strong entries");
        assert_eq!(refs.weak_count(), 1);
        let mut roots = Vec::new();
        refs.collect_roots(&mut roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(refs.resolve(w), Some(obj));

        let moved_to = a + 0x100;
        refs.update_after_gc(&cratonvm_types::PointerMap::from_iter([(a, moved_to)]));
        assert_eq!(refs.resolve(s).map(|o| o.as_ptr() as usize), Some(moved_to));
        assert_eq!(
            refs.resolve(w),
            Some(obj),
            "the weak entry is the sweep's, not update_after_gc's"
        );

        // A verdict applies only while the entry still holds the judged address.
        refs.apply_weak_verdicts(&[((w & !1) as usize, a + 8, 0)]);
        assert_eq!(refs.resolve(w), Some(obj), "stale verdict ignored");
        refs.apply_weak_verdicts(&[((w & !1) as usize, a, 0)]);
        assert_eq!(refs.resolve(w), None, "cleared");
        assert_eq!(refs.kind(w), Some(JniGlobalKind::Weak), "still a valid handle");
        assert!(refs.weak_snapshot().is_empty(), "a cleared entry is not swept again");

        assert!(refs.remove(w));
        assert_eq!(refs.kind(w), None);
        assert!(!refs.remove(w));
        // Drop frees whatever is left, weak entries included.
        let _w2 = refs.add_weak(obj);
    }

    /// gc-common w8-c: the three out-of-epilogue clears on a live VM's table.
    /// The remark clear drops exactly the unmarked referents not in `keep`;
    /// the span clear drops exactly the referents inside a freed span; the
    /// in-place sweep drops a referent in reclaimed space (the inactive young
    /// semispace) and keeps a live one. The collection sweep remaps a moved
    /// referent onto a genuine copy.
    #[test]
    fn weak_global_clears_outside_the_collection_epilogue() {
        use crate::config::{GcAlgorithm, VmConfig};
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let alloc = || heap.alloc_object(ClassId::new(0), 1);
        let (a, b, c, d) = (alloc(), alloc(), alloc(), alloc());
        let at = |o: ObjectRef| o.as_ptr() as usize;
        let (wa, wb, wc, wd) = {
            let mut refs = shared.natives.jni_global_refs.lock();
            (refs.add_weak(a), refs.add_weak(b), refs.add_weak(c), refs.add_weak(d))
        };
        let resolve = |h: JObject| shared.natives.jni_global_refs.lock().resolve(h);

        // Remark: `a` marked, `b` unmarked but about to be resurrected, `c`
        // and `d` unmarked.
        let n = clear_weak_global_refs_unmarked(&shared, &|x| x == at(a), &[at(b)]);
        assert_eq!(n, 2);
        assert_eq!(resolve(wa), Some(a));
        assert_eq!(resolve(wb), Some(b), "a resurrected finalizable keeps its weak global");
        assert_eq!(resolve(wc), None);
        assert_eq!(resolve(wd), None);

        // Freed spans: only `b`'s.
        assert_eq!(clear_weak_global_refs_in_spans(&shared, &[]), 0);
        assert_eq!(clear_weak_global_refs_in_spans(&shared, &[(at(b), 8)]), 1);
        assert_eq!(resolve(wb), None);
        assert_eq!(resolve(wa), Some(a));

        // The epilogue sweep follows a relocation onto a genuine copy.
        let copy = alloc();
        let stats = sweep_weak_global_refs(
            &shared,
            &cratonvm_types::PointerMap::from_iter([(at(a), at(copy))]),
        );
        assert_eq!(stats.moved, 1, "{stats:?}");
        assert_eq!(resolve(wa), Some(copy));

        // In place: a referent in the inactive semispace is reclaimed space.
        let (inactive_lo, _) = heap
            .young_inactive_semispace_range()
            .expect("the Generational heap has a semispace pair");
        let wz = shared
            .natives
            .jni_global_refs
            .lock()
            .add_weak(unsafe { ObjectRef::from_raw(inactive_lo as *mut u8) });
        let cleared = sweep_weak_global_refs_in_place(&shared);
        assert!(cleared >= 1);
        assert_eq!(resolve(wz), None, "a referent in reclaimed space reads NULL");
        if heap.is_object_address(at(copy)).is_some() {
            assert_eq!(resolve(wa), Some(copy), "a live referent survives the in-place sweep");
        }
    }

    /// gc-common w7-c: the class-dependent half of the indirect-locals
    /// evidence. With `CRATONVM_JNI_INDIRECT_LOCALS` on: FindClass,
    /// GetMethodID, `NewObjectA` (the handle survives `<init>`),
    /// `CallObjectMethodA` with an indirect argument and an indirect result,
    /// `CallIntMethodA`, GetFieldID / GetObjectField / SetObjectField, and a
    /// relocation of a method's result onto a genuine copy.
    #[test]
    fn indirect_locals_through_constructors_fields_and_calls() {
        use crate::config::VmConfig;
        use crate::vm::Vm;
        let addr = |o: ObjectRef| o.as_ptr() as usize;
        let mut vm = Vm::new(VmConfig::default());
        let _jni = JniTls::replace(&vm.shared).with_thread(vm.main_thread.as_mut() as *mut _);
        let _ = take_jni_pending_exception();
        let env = get_jni_env();
        let on = IndirectLocalsOn::new();
        let base = local_frame_depth();
        push_local_frame(16); // the native's implicit dispatch frame
        let c = |s: &str| CString::new(s).unwrap();

        let sb_class = jni_find_class(env, c("java/lang/StringBuilder").as_ptr());
        assert_ne!(sb_class, 0);
        let init = jni_get_method_id(env, sb_class, c("<init>").as_ptr(), c("()V").as_ptr());
        if init == 0 {
            // The unit-test VM has no JDK class files: `StringBuilder` is a
            // `CompatibilityStub` with no methods there (w7 orchestrator), so
            // this half of the evidence needs a JDK-backed run. Skip, loudly.
            eprintln!(
                "indirect_locals_through_constructors_fields_and_calls: SKIPPED \
                 (java/lang/StringBuilder has no <init>()V in this VM: no JDK class files)"
            );
            truncate_local_frames(base);
            drop(on);
            return;
        }
        let sb = jni_new_object_a(env, sb_class, init, std::ptr::null());
        assert!(is_indirect_local_tag(sb), "NewObjectA hands out an indirect local");
        let s = jni_new_string_utf(env, c("w7c").as_ptr());
        let append = jni_get_method_id(
            env,
            sb_class,
            c("append").as_ptr(),
            c("(Ljava/lang/String;)Ljava/lang/StringBuilder;").as_ptr(),
        );
        assert_ne!(append, 0);
        let args = [JValue { l: s }];
        let returned = jni_call_object_method_a(env, sb, append, args.as_ptr());
        assert!(is_indirect_local_tag(returned));
        assert_eq!(jni_is_same_object(env, returned, sb), JNI_TRUE, "append returns this");
        let length = jni_get_method_id(env, sb_class, c("length").as_ptr(), c("()I").as_ptr());
        assert_eq!(jni_call_int_method_a(env, sb, length, std::ptr::null()), 3);
        let to_string = jni_get_method_id(
            env,
            sb_class,
            c("toString").as_ptr(),
            c("()Ljava/lang/String;").as_ptr(),
        );
        let text = jni_call_object_method_a(env, sb, to_string, std::ptr::null());
        assert!(is_indirect_local_tag(text));
        assert_eq!(jstring_text(env, text).as_deref(), Some("w7c"));

        // An inherited object field (AbstractStringBuilder.value).
        let value = jni_get_field_id(env, sb_class, c("value").as_ptr(), c("[B").as_ptr());
        assert_ne!(value, 0);
        let bytes = jni_get_object_field(env, sb, value);
        assert!(is_indirect_local_tag(bytes));
        assert!(jni_get_array_length(env, bytes) >= 3);
        jni_set_object_field(env, sb, value, bytes);
        let bytes_again = jni_get_object_field(env, sb, value);
        assert_eq!(jni_is_same_object(env, bytes_again, bytes), JNI_TRUE);

        // A collection moves `toString()`'s result onto a genuine copy: the
        // native's unchanged handle follows it.
        let twin = jni_new_string_utf(env, c("w7c").as_ptr());
        assert_eq!(jni_is_same_object(env, twin, text), JNI_FALSE, "a new String");
        let from = jobject_to_obj(text).map(addr).unwrap();
        let to = jobject_to_obj(twin).map(addr).unwrap();
        update_local_refs_after_gc(&cratonvm_types::PointerMap::from_iter([(from, to)]));
        assert_eq!(jobject_to_obj(text).map(addr), Some(to));
        assert_eq!(jni_is_same_object(env, text, twin), JNI_TRUE);
        assert_eq!(jstring_text(env, text).as_deref(), Some("w7c"));

        truncate_local_frames(base);
        drop(on);
    }

    /// gc-common w7-c: `PopLocalFrame` pops only a frame native code pushed
    /// (an unbalanced pop used to pop the VM's implicit dispatch frame, then
    /// the caller's), and a VM scope's exit closes a frame the native leaked.
    #[test]
    fn pop_local_frame_never_pops_a_vm_frame_and_scope_exit_closes_leaks() {
        let env = get_jni_env();
        let base = local_frame_depth();
        push_local_frame(4); // the implicit dispatch frame
        record_local_handle(0x1000);
        assert!(!top_local_frame_is_explicit());
        assert_eq!(jni_pop_local_frame(env, 0x2000), 0x2000, "result unchanged");
        assert_eq!(local_frame_depth(), base + 1, "the implicit frame stays open");

        assert_eq!(jni_push_local_frame(env, 4), JNI_OK);
        assert!(top_local_frame_is_explicit());
        record_local_handle(0x3000);
        let _ = jni_pop_local_frame(env, 0);
        assert_eq!(local_frame_depth(), base + 1, "a balanced pop pops the pushed frame");
        assert!(!top_local_frame_is_explicit());
        let _ = jni_pop_local_frame(env, 0);
        assert_eq!(local_frame_depth(), base + 1, "and no further");

        // A leaked PushLocalFrame: the scope's exit closes both frames.
        assert_eq!(jni_push_local_frame(env, 4), JNI_OK);
        assert_eq!(jni_push_local_frame(env, 4), JNI_OK);
        truncate_local_frames(base);
        assert_eq!(local_frame_depth(), base);
        assert!(JNI_EXPLICIT_FRAME_DEPTHS.with(|d| d.borrow().iter().all(|&x| x < base)));
    }

    /// gc-common w7-c: `NewStringUTF` / `NewString` build a NEW String each
    /// time, as HotSpot does, and never put it in the interned pool, which
    /// `roots.rs` section 5 roots strongly (every JNI-built string used to be
    /// immortal).
    #[test]
    fn jni_strings_are_new_and_not_interned() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        let text = CString::new("w7-c never interned").unwrap();
        let a = jni_new_string_utf(env, text.as_ptr());
        let b = jni_new_string_utf(env, text.as_ptr());
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_eq!(jni_is_same_object(get_jni_env(), a, b), JNI_FALSE);
        let units: Vec<u16> = "w7-c never interned".encode_utf16().collect();
        let c = jni_new_string(env, units.as_ptr(), units.len() as JSize);
        assert_eq!(jni_is_same_object(get_jni_env(), a, c), JNI_FALSE);
        assert_eq!(jstring_text(env, c).as_deref(), Some("w7-c never interned"));
        assert!(
            shared.mem.string_pool.read().get("w7-c never interned").is_none(),
            "a JNI string must not enter the strongly rooted intern pool"
        );
    }

    /// Round 11 wave 13 (lane rt): an array's header carries its COMPONENT's
    /// class id. `IsInstanceOf` and `GetObjectClass` read that id as the
    /// array's own class, so a `String[]` was an instance of `String` and
    /// reported `String` as its class.
    #[test]
    fn jni_an_array_is_not_an_instance_of_its_component() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let text = CString::new("r11w13 component").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        assert_ne!(s, 0);
        let string_class = jni_get_object_class(env, s);
        assert_ne!(string_class, 0);
        assert_eq!(jni_is_instance_of(env, s, string_class), JNI_TRUE);
        let strings = jni_new_object_array(env, 2, string_class, 0);
        assert_ne!(strings, 0);
        assert_eq!(
            jni_is_instance_of(env, strings, string_class),
            JNI_FALSE,
            "a String[] is not a String"
        );
        let array_class = jni_get_object_class(env, strings);
        assert_ne!(array_class, 0);
        assert_ne!(
            array_class, string_class,
            "GetObjectClass of a String[] must not answer its component"
        );
        assert_eq!(
            jni_is_instance_of(env, strings, array_class),
            JNI_TRUE,
            "an array is an instance of the class GetObjectClass reports for it"
        );
        clear_jni_context();
    }

    /// Round 11 wave 15 (lane rt): `IsAssignableFrom` between two ARRAY
    /// classes. `String[]` is assignable to `Object[]` (covariance), and not
    /// the other way round. The walk and `class_is_subtype` see only an array
    /// class's own `Object`/`Cloneable`/`Serializable` entries, so the first
    /// answer was `JNI_FALSE` before the array rule was consulted.
    #[test]
    fn jni_is_assignable_from_honours_array_covariance() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let text = CString::new("r11w15 covariance").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        assert_ne!(s, 0);
        let string_class = jni_get_object_class(env, s);
        let object_class = jni_get_superclass(env, string_class);
        assert_ne!(object_class, 0, "String has a superclass");
        let strings = jni_new_object_array(env, 1, string_class, 0);
        let objects = jni_new_object_array(env, 1, object_class, 0);
        assert!(strings != 0 && objects != 0);
        let string_array_class = jni_get_object_class(env, strings);
        let object_array_class = jni_get_object_class(env, objects);
        let name_of = |c: JClass| {
            shared
                .classes
                .class_manager
                .read()
                .get_class(jclass_class_id(c))
                .map(|k| k.name.to_string())
                .unwrap_or_default()
        };
        // `GetObjectClass` falls back to `java/lang/Object` when an array class
        // cannot be loaded in this bare VM; the relation is only meaningful
        // between two real array classes.
        let both_arrays = name_of(string_array_class).starts_with('[')
            && name_of(object_array_class).starts_with('[');
        if both_arrays {
            assert_eq!(
                jni_is_assignable_from(env, string_array_class, object_array_class),
                JNI_TRUE,
                "String[] is assignable to Object[]"
            );
            assert_eq!(
                jni_is_assignable_from(env, object_array_class, string_array_class),
                JNI_FALSE,
                "Object[] is not assignable to String[]"
            );
        }
        assert_eq!(
            jni_is_assignable_from(env, string_array_class, object_class),
            JNI_TRUE,
            "every array is assignable to Object"
        );
        clear_jni_context();
    }

    #[test]
    fn is_same_object_via_global_ref() {
        // IsSameObject must return true when comparing a local ref and a global ref
        // to the same underlying object. NEW-11: same self-inconsistency
        // fix as `global_ref_jobject_to_obj_roundtrip`.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let local_ref = obj_to_jobject(obj); // raw local ref
        let global_ref = shared.natives.jni_global_refs.lock().add(obj);
        // Both refer to the same object → IsSameObject must return true.
        let resolved_local = jobject_to_obj(local_ref).unwrap();
        let resolved_global = jobject_to_obj(global_ref).unwrap();
        assert_eq!(resolved_local, resolved_global);
    }

    #[test]
    fn new_string_utf16_roundtrip() {
        // NewString (UTF-16) must produce the same Java String as NewStringUTF.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        let utf16: Vec<u16> = "hello".encode_utf16().collect();
        let jstr = jni_new_string(env, utf16.as_ptr(), utf16.len() as JSize);
        // Both GetStringUTFLength and GetStringLength must reflect the content.
        let utf_len = jni_get_string_utf_length(env, jstr);
        let char_len = jni_get_string_length(env, jstr);
        assert_eq!(utf_len, 5, "UTF byte length of 'hello'");
        assert_eq!(char_len, 5, "UTF-16 code unit count of 'hello'");
    }

    #[test]
    fn alloc_object_returns_non_null() {
        // AllocObject on a valid class must return a non-zero handle.
        let env = get_jni_env();
        // Class slot 0 is synthetic and may have 0 fields — still a valid alloc target.
        let handle = jni_alloc_object(env, 0);
        // Class 0 may not be resolvable → result is 0; just check no crash.
        let _ = handle;
    }

    #[test]
    fn set_static_boolean_byte_char_short_table_slots() {
        // Table slots 155-158 must be non-stub function pointers.
        let env = get_jni_env();
        let check = |slot: usize| {
            let func_ptr = unsafe { *(*env).add(slot) };
            assert_ne!(func_ptr, 0, "slot {slot} must not be null");
        };
        check(155); // SetStaticBooleanField
        check(156); // SetStaticByteField
        check(157); // SetStaticCharField
        check(158); // SetStaticShortField
        check(28); // AllocObject
        check(30); // NewObjectA
        check(163); // NewString
        check(164); // GetStringLength
        check(165); // GetStringChars
        check(166); // ReleaseStringChars
    }

    // -----------------------------------------------------------------------
    // JniGlobalRefs: add / remove / resolve / count / collect_roots / update_after_gc
    // -----------------------------------------------------------------------

    fn alloc_test_obj() -> (Arc<crate::vm::SharedVm>, ObjectRef) {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        (shared, obj)
    }

    #[test]
    fn global_refs_resolve_returns_none_for_null_handle() {
        let refs = JniGlobalRefs::new();
        assert_eq!(refs.resolve(0), None);
    }

    #[test]
    fn global_refs_resolve_invalid_handle_not_tagged() {
        let refs = JniGlobalRefs::new();
        // A handle with bit 0 clear is not a global ref.
        assert_eq!(refs.resolve(0x1000), None);
    }

    #[test]
    fn global_refs_resolve_stale_handle() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        refs.remove(handle);
        // After removal, resolve must return None.
        assert_eq!(refs.resolve(handle), None);
    }

    #[test]
    fn global_refs_count_tracks_multiple() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let h1 = refs.add(obj);
        let h2 = refs.add(obj);
        assert_eq!(refs.count(), 2);
        refs.remove(h1);
        assert_eq!(refs.count(), 1);
        refs.remove(h2);
        assert_eq!(refs.count(), 0);
    }

    #[test]
    fn global_refs_collect_roots() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        refs.add(obj);
        refs.add(obj);
        let mut roots = Vec::new();
        refs.collect_roots(&mut roots);
        assert_eq!(roots.len(), 2);
        for r in &roots {
            assert_eq!(r.as_ptr(), obj.as_ptr());
        }
    }

    #[test]
    fn global_refs_update_after_gc() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        let old_addr = obj.as_ptr() as usize;
        // Simulate GC moving the object to a new address.
        let new_addr = old_addr.wrapping_add(0x1000);
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr, new_addr);
        refs.update_after_gc(&pointer_map);
        let resolved = refs.resolve(handle).expect("handle should still resolve");
        assert_eq!(resolved.as_ptr() as usize, new_addr);
    }

    #[test]
    fn global_refs_remove_returns_false_for_untagged() {
        let mut refs = JniGlobalRefs::new();
        assert!(!refs.remove(0x1000)); // bit 0 clear
    }

    // -----------------------------------------------------------------------
    // vm-jni-roots #1: JNI LOCAL refs are GC roots and are remapped on move.
    //
    // These exercise the pure thread-local-frame bookkeeping without a heap:
    // `collect_local_ref_roots` / `update_local_refs_after_gc` only read/write
    // raw handle integers (and wrap them with `ObjectRef::from_raw`, which does
    // not dereference), so fabricated aligned, non-null, bit-0-clear handles
    // are sufficient. Each test pops its frame at the end so it leaves no
    // residue for sibling tests sharing the thread-local stack.
    // -----------------------------------------------------------------------

    #[test]
    fn local_refs_collected_as_roots() {
        push_local_frame(8);
        // Two valid local-ref handles (8-byte aligned, non-null, bit 0 == 0).
        let h1: JObject = 0x1_0000;
        let h2: JObject = 0x2_0000;
        track_local_ref(h1);
        track_local_ref(h2);
        // A global ref (bit 0 == 1) and null must NOT be picked up here.
        track_local_ref(0x3_0001);
        track_local_ref(0);

        let mut roots = Vec::new();
        collect_local_ref_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert!(addrs.contains(&(h1 as usize)));
        assert!(addrs.contains(&(h2 as usize)));
        assert!(!addrs.contains(&0x3_0001));
        assert_eq!(addrs.len(), 2, "only the 2 untagged local refs are roots");

        pop_local_frame(0);
    }

    #[test]
    fn local_refs_remapped_after_gc() {
        push_local_frame(8);
        let old_addr: JObject = 0x4_0000;
        track_local_ref(old_addr);
        let new_addr = (old_addr as usize).wrapping_add(0x1000);
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr as usize, new_addr);

        update_local_refs_after_gc(&pointer_map);

        let mut roots = Vec::new();
        collect_local_ref_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert!(
            addrs.contains(&new_addr),
            "moved local ref must report its relocated address"
        );
        assert!(
            !addrs.contains(&(old_addr as usize)),
            "stale from-space address must be gone after remap"
        );

        pop_local_frame(0);
    }

    #[test]
    fn local_refs_dropped_when_frame_popped() {
        push_local_frame(4);
        track_local_ref(0x5_0000);
        pop_local_frame(0);
        // After popping the only frame there are no local-ref roots.
        let mut roots = Vec::new();
        collect_local_ref_roots(&mut roots);
        assert!(roots.iter().all(|r| r.as_ptr() as usize != 0x5_0000));
    }

    // -----------------------------------------------------------------------
    // vm-jni-roots #2: Get/Release<Type>ArrayElements free with the STORED
    // length/capacity, never a length re-derived from the array handle.
    //
    // We mirror the exact pattern the macros use — `Vec::with_capacity` +
    // `forget`, record `(len, cap)` in the VM's `JniGlobalRefs::elem_copies`
    // (gc-common w10-c; a thread-local before), then look it up and
    // `Vec::from_raw_parts(ptr, len, cap)` — and prove the round-trip is
    // sound even when a (simulated) handle-derived length would DIFFER. With
    // the old re-derivation, a divergent length here produced a from_raw_parts
    // mismatch (heap corruption); with the stored layout it is always exact.
    // (Miri/ASAN would flag any mismatch; a normal build at least proves the
    // map plumbing compiles and the lengths are preserved.)
    // -----------------------------------------------------------------------
    #[test]
    fn array_elem_buffer_freed_with_stored_layout() {
        let mut buf: Vec<i32> = Vec::with_capacity(5);
        for i in 0..5 {
            buf.push(i);
        }
        let ptr = buf.as_mut_ptr();
        let stored = ArrayElemBuffer {
            len: buf.len(),
            cap: buf.capacity(),
            // No live VM here; this test exercises only the layout plumbing, so
            // a sentinel global-ref handle is fine (never resolved).
            array_gref: 0,
        };
        std::mem::forget(buf);
        let mut refs = JniGlobalRefs::new();
        refs.elem_copies.insert(ptr as usize, stored);

        // JNI_COMMIT peeks and leaves the record in place.
        assert!(refs.elem_copy(ptr as usize, false).is_some());
        assert_eq!(refs.outstanding_copies(), 1);
        // Simulate a Release that does NOT trust the handle: pull the stored
        // layout and reconstruct exactly. A spurious "handle length" of 0 or 99
        // must be irrelevant.
        let entry = refs.elem_copy(ptr as usize, true);
        let ArrayElemBuffer { len, cap, .. } = entry.expect("buffer must be tracked");
        assert_eq!(len, 5);
        assert_eq!(cap, 5);
        // Sound free using the stored layout (NOT a re-derived length).
        unsafe {
            drop(Vec::from_raw_parts(ptr, len, cap));
        }
        // Entry is consumed; a second release would find nothing and no-op.
        assert!(refs.elem_copy(ptr as usize, true).is_none());
        assert_eq!(refs.outstanding_copies(), 0);
    }

    // -----------------------------------------------------------------------
    // encode / decode method_id roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn method_id_roundtrip() {
        let cid = crate::classloading::ClassId::new(42);
        let method_index: u16 = 7;
        let mid = encode_method_id(cid, method_index);
        let (decoded_cid, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, method_index);
    }

    #[test]
    fn method_id_roundtrip_zero() {
        let cid = crate::classloading::ClassId::new(0);
        let mid = encode_method_id(cid, 0);
        let (decoded_cid, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, 0);
    }

    #[test]
    fn method_id_roundtrip_large_values() {
        let cid = crate::classloading::ClassId::new(0xFFFF_FFFF);
        let method_index: u16 = 0xFFFF;
        let mid = encode_method_id(cid, method_index);
        let (decoded_cid, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, method_index);
    }

    // -----------------------------------------------------------------------
    // encode / decode field_id roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn field_id_roundtrip() {
        let cid = crate::classloading::ClassId::new(99);
        let field_index: usize = 5;
        let fid = encode_field_id(cid, field_index);
        let (decoded_cid, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, field_index);
    }

    #[test]
    fn field_id_roundtrip_zero() {
        let cid = crate::classloading::ClassId::new(0);
        let fid = encode_field_id(cid, 0);
        let (decoded_cid, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, 0);
    }

    #[test]
    fn field_id_roundtrip_large_class_id() {
        let cid = crate::classloading::ClassId::new(0xDEAD_BEEF);
        let field_index: usize = 1234;
        let fid = encode_field_id(cid, field_index);
        let (decoded_cid, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, field_index);
    }

    // -----------------------------------------------------------------------
    // parse_param_types_inner
    // -----------------------------------------------------------------------

    #[test]
    fn parse_param_types_single_int() {
        // (I)V -> [b'I']
        assert_eq!(parse_param_types_inner("(I)V"), vec![b'I']);
    }

    #[test]
    fn parse_param_types_mixed() {
        // (ILjava/lang/String;[BZ)V -> [I, L, [, Z]
        let result = parse_param_types_inner("(ILjava/lang/String;[BZ)V");
        assert_eq!(result, vec![b'I', b'L', b'[', b'Z']);
    }

    #[test]
    fn parse_param_types_empty() {
        // ()V -> []
        assert_eq!(parse_param_types_inner("()V"), Vec::<u8>::new());
    }

    #[test]
    fn parse_param_types_long_double() {
        // (JD)F -> [J, D]
        assert_eq!(parse_param_types_inner("(JD)F"), vec![b'J', b'D']);
    }

    #[test]
    fn parse_param_types_arrays() {
        // ([I[Ljava/lang/Object;)V -> [[, []
        let result = parse_param_types_inner("([I[Ljava/lang/Object;)V");
        assert_eq!(result, vec![b'[', b'[']);
    }

    // -----------------------------------------------------------------------
    // parse_param_types_cached consistent with inner
    // -----------------------------------------------------------------------

    #[test]
    fn parse_param_types_cached_matches_inner() {
        let descriptors = [
            "(I)V",
            "(ILjava/lang/String;[BZ)V",
            "()V",
            "(JD)F",
            "([I[Ljava/lang/Object;)V",
        ];
        for desc in &descriptors {
            assert_eq!(
                parse_param_types_cached(desc),
                parse_param_types_inner(desc),
                "cached and inner disagree on {desc}"
            );
        }
        // Call again to exercise the cache hit path.
        for desc in &descriptors {
            assert_eq!(
                parse_param_types_cached(desc),
                parse_param_types_inner(desc),
                "cached hit disagree on {desc}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // DescriptorCache (bounded LRU)
    // -----------------------------------------------------------------------

    #[test]
    fn descriptor_cache_get_hit_and_miss() {
        let mut cache = DescriptorCache::new();
        assert_eq!(cache.get("(I)V"), None);
        cache.insert("(I)V", b"I".to_vec());
        assert_eq!(cache.get("(I)V"), Some(&b"I"[..]));
        assert_eq!(cache.get("(J)V"), None);
    }

    #[test]
    fn descriptor_cache_evicts_one_not_all_when_full() {
        let mut cache = DescriptorCache::new();
        // Fill to capacity with unique descriptors.
        for i in 0..DESCRIPTOR_CACHE_CAPACITY {
            cache.insert(&format!("(I{i})V"), vec![b'I']);
        }
        assert_eq!(cache.map.len(), DESCRIPTOR_CACHE_CAPACITY);

        // Touch every entry except the first so it becomes the LRU victim.
        for i in 1..DESCRIPTOR_CACHE_CAPACITY {
            assert!(cache.get(&format!("(I{i})V")).is_some());
        }

        // Insert one more: exactly one entry is evicted (the untouched LRU),
        // NOT the whole cache — hot entries survive.
        cache.insert("(NEW)V", vec![b'L']);
        assert_eq!(cache.map.len(), DESCRIPTOR_CACHE_CAPACITY);
        assert_eq!(cache.get("(I0)V"), None, "LRU entry should be evicted");
        assert!(cache.get("(NEW)V").is_some(), "new entry present");
        assert!(
            cache.get("(I1)V").is_some(),
            "recently-used entry must survive (no full flush)"
        );
    }

    #[test]
    fn descriptor_cache_reinsert_existing_key_no_eviction() {
        let mut cache = DescriptorCache::new();
        for i in 0..DESCRIPTOR_CACHE_CAPACITY {
            cache.insert(&format!("(I{i})V"), vec![b'I']);
        }
        // Re-inserting an existing key must not evict — it overwrites in place.
        cache.insert("(I0)V", b"J".to_vec());
        assert_eq!(cache.map.len(), DESCRIPTOR_CACHE_CAPACITY);
        assert_eq!(cache.get("(I0)V"), Some(&b"J"[..]));
    }

    // -----------------------------------------------------------------------
    // jni_get_object_ref_type
    // -----------------------------------------------------------------------

    #[test]
    fn get_object_ref_type_null() {
        let env = get_jni_env();
        assert_eq!(jni_get_object_ref_type(env, 0), 0); // JNIInvalidRefType
    }

    #[test]
    fn get_object_ref_type_global() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        let env = get_jni_env();
        assert_eq!(jni_get_object_ref_type(env, handle), 2); // JNIGlobalRefType
    }

    #[test]
    fn get_object_ref_type_local() {
        let (_shared, obj) = alloc_test_obj();
        // Local ref is a raw pointer with bit 0 clear.
        let local = obj_to_jobject(obj);
        let env = get_jni_env();
        assert_eq!(jni_get_object_ref_type(env, local), 1); // JNILocalRefType
    }

    // --- Phase 80.2: JNI Context Arc ref-counting tests ---

    #[test]
    fn jni_context_arc_keeps_vm_alive() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let weak = Arc::downgrade(&shared);
        let _jni = JniTls::cleanup_only();
        set_jni_context_arc(shared.clone());
        drop(shared);
        assert!(weak.upgrade().is_some(), "TLS Arc must keep SharedVm alive");
        clear_jni_context();
        assert!(
            weak.upgrade().is_none(),
            "SharedVm must be dropped after clear"
        );
    }

    #[test]
    fn jni_context_clear_drops_arc() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(Arc::strong_count(&shared), 1);
        let _jni = JniTls::cleanup_only();
        set_jni_context_arc(shared.clone());
        assert_eq!(Arc::strong_count(&shared), 2, "TLS must hold an Arc");
        clear_jni_context();
        assert_eq!(Arc::strong_count(&shared), 1, "clear must drop TLS Arc");
    }

    // -----------------------------------------------------------------------
    // Session 44: JNI Completeness — new function tests
    // -----------------------------------------------------------------------

    /// Slot 233 (`GetModule`, JNI 9+) is wired to its implementation.
    ///
    /// ASSERTS EQUALITY WITH THE TARGET, NOT INEQUALITY WITH THE STUB, and the
    /// difference is not stylistic. `assert_ne!(table[233], jni_stub as usize)`
    /// is what this test used to say, and it fails in release builds — not
    /// because the slot is unwired (it is assigned exactly once, from
    /// `jni_get_module`, in `build_function_table`), but because
    /// **`jni_get_module` and `jni_stub` resolve to the SAME ADDRESS**. Both
    /// are `extern "C"` functions that return a constant 0 and touch none of
    /// their arguments, and nothing obliges the toolchain to keep two such
    /// functions at distinct addresses: MSVC's `/OPT:ICF` folds identical code,
    /// and the release profile's `lto = "fat"` + `codegen-units = 1` gives it
    /// the whole program to fold across. Measured: both sides printed
    /// `140702574873248`.
    ///
    /// So the old assertion asked a question with no answer in this binary.
    /// "Is slot 233 the real GetModule or the generic stub?" is undecidable by
    /// address when the real GetModule *is* a stub in all but name — and it
    /// would stay undecidable by behaviour too, since both return 0. What IS
    /// decidable, and is the invariant worth policing, is that the slot names
    /// `jni_get_module`. That holds whether or not the linker folds.
    ///
    /// The failure was release-only and CI runs `cargo test -p cratonvm-vm
    /// --lib` in debug, which is why it never showed up there.
    ///
    /// # This test and the other two address-comparing ones are green under the
    /// repo's release profile, and RED under `CARGO_PROFILE_RELEASE_LTO=thin`
    ///
    /// Measured 2026-08-06. `lto = "fat"` + `codegen-units = 1` (what
    /// `[profile.release]` says, and what CI runs) passes. Overriding to
    /// `LTO=thin` + `CODEGEN_UNITS=16` — the documented workaround when the fat
    /// link is OOM-killed on a loaded build host — fails this,
    /// `jni_nio_slots_not_stub` and `jni_function_table_matches_the_jni_h_layout`,
    /// because thin LTO's function merging can leave one of two identical-bodied
    /// `extern "C"` stubs as a THUNK: the address `build_function_table` stored
    /// and the address `f as *const ()` yields here are then two different
    /// entry points into the same code.
    ///
    /// That is a property of the override, not of the table. **Do not "fix" it
    /// by weakening the assertion** — and do not read a red run under that
    /// override as a defect on `dev`. Re-run with the real profile first.
    #[test]
    fn jni_function_table_extended_to_234() {
        let env = get_jni_env();
        // The table is `[usize; JNI_FUNCTION_COUNT]`, so reading slot 235 at
        // all requires the table to be at least 236 entries long — 235 is the
        // last slot JDK 25's `jni.h` declares, and a native compiled against
        // that header will load it.
        assert_eq!(
            JNI_FUNCTION_COUNT, 236,
            "the table must cover every slot JDK 25's jni.h declares"
        );
        let func_ptr = unsafe { *(*env).add(233) };
        assert_eq!(
            func_ptr, jni_get_module as *const () as usize,
            "slot 233 must dispatch to GetModule"
        );
    }

    #[test]
    fn jni_direct_byte_buffer_roundtrip() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        // Allocate a native buffer
        let mut native_buf: Vec<u8> = vec![0u8; 1024];
        let addr = native_buf.as_mut_ptr();
        let capacity = 1024i64;
        // Create a direct byte buffer
        let buf = jni_new_direct_byte_buffer(env, addr, capacity);
        assert_ne!(buf, 0, "NewDirectByteBuffer must return non-null");
        // Get the address back
        let retrieved_addr = jni_get_direct_buffer_address(env, buf);
        assert_eq!(retrieved_addr, addr, "GetDirectBufferAddress must match");
        // Get the capacity back
        let retrieved_cap = jni_get_direct_buffer_capacity(env, buf);
        assert_eq!(
            retrieved_cap, capacity,
            "GetDirectBufferCapacity must match"
        );
    }

    /// Write `value` into whichever slot the direct-buffer accessors treat as
    /// authoritative for `field` on `obj` — the same resolution
    /// `jni_new_direct_byte_buffer` performs, so the test cannot drift away
    /// from production if the layout choice changes.
    fn poke_dbb_slot(shared: &SharedVm, obj: ObjectRef, field: &str, value: Value) {
        let class_id = shared.mem.heap.class_id_of(obj);
        let idx = match (dbb_slots(shared, class_id), field) {
            (Some((a, _)), "address") => a,
            (Some((_, c)), "capacity") => c,
            (None, "address") => 0,
            (None, "capacity") => 1,
            _ => unreachable!("unknown direct-buffer field {field}"),
        };
        shared.mem.heap.set_field(obj, idx, value);
    }

    /// PROCESS-GLOBAL-STATE ROUND 2 regression: `GetDirectBufferAddress` must
    /// read the buffer OBJECT, never a side table keyed on the object's raw
    /// heap address.
    ///
    /// The removed `DIRECT_BUFFERS` map was keyed on `obj_to_jobject(obj)` and
    /// consulted before the field read, so it answered from the address alone.
    /// Once a moving collection recycled a dead buffer's address, the *next*
    /// object at that address inherited the dead buffer's `malloc` pointer.
    /// This test drives the same divergence deterministically: change what the
    /// object says without touching the handle. With the cache present the
    /// getter returned the stale pointer; it must now follow the object.
    #[test]
    fn jni_direct_buffer_address_follows_the_object_not_a_cache() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();

        let mut first: Vec<u8> = vec![0u8; 256];
        let mut second: Vec<u8> = vec![0u8; 512];
        let first_addr = first.as_mut_ptr();
        let second_addr = second.as_mut_ptr();
        assert_ne!(first_addr, second_addr);

        let buf = jni_new_direct_byte_buffer(env, first_addr, 256);
        assert_ne!(buf, 0);
        assert_eq!(jni_get_direct_buffer_address(env, buf), first_addr);

        // Re-point the OBJECT at a different allocation, leaving the handle
        // (the old cache key) untouched.
        let oref = jobject_to_obj(buf).expect("handle must resolve");
        poke_dbb_slot(&shared, oref, "address", Value::Long(second_addr as i64));
        poke_dbb_slot(&shared, oref, "capacity", Value::Int(512));

        assert_eq!(
            jni_get_direct_buffer_address(env, buf),
            second_addr,
            "GetDirectBufferAddress must read the object's address field, not a \
             cache keyed on the object's raw heap address"
        );
        assert_eq!(
            jni_get_direct_buffer_capacity(env, buf),
            512,
            "GetDirectBufferCapacity must read the object's capacity field"
        );

        // A zeroed address field (what a recycled slot looks like) must produce
        // a null answer, not a stale `malloc` pointer.
        poke_dbb_slot(&shared, oref, "address", Value::Long(0));
        assert!(
            jni_get_direct_buffer_address(env, buf).is_null(),
            "a cleared address field must read back as null"
        );
    }

    /// Two live direct buffers must not cross-talk. With the address-keyed
    /// cache this held only by luck of allocation; with per-object fields it is
    /// structural.
    #[test]
    fn jni_direct_buffers_are_independent() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();

        let mut a: Vec<u8> = vec![0u8; 64];
        let mut b: Vec<u8> = vec![0u8; 128];
        let a_addr = a.as_mut_ptr();
        let b_addr = b.as_mut_ptr();

        let buf_a = jni_new_direct_byte_buffer(env, a_addr, 64);
        let buf_b = jni_new_direct_byte_buffer(env, b_addr, 128);
        assert_ne!(buf_a, 0);
        assert_ne!(buf_b, 0);
        assert_ne!(buf_a, buf_b, "two buffers must be distinct objects");

        assert_eq!(jni_get_direct_buffer_address(env, buf_a), a_addr);
        assert_eq!(jni_get_direct_buffer_address(env, buf_b), b_addr);
        assert_eq!(jni_get_direct_buffer_capacity(env, buf_a), 64);
        assert_eq!(jni_get_direct_buffer_capacity(env, buf_b), 128);
    }

    /// A zero capacity is legal (`NewDirectByteBuffer(addr, 0)`), so the
    /// capacity getter must not use "non-zero means present" as its presence
    /// test the way the address getter does.
    #[test]
    fn jni_direct_buffer_zero_capacity_round_trips() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();

        let mut backing = [0u8; 8];
        let addr = backing.as_mut_ptr();
        let buf = jni_new_direct_byte_buffer(env, addr, 0);
        assert_ne!(buf, 0, "zero capacity is legal and must allocate a buffer");
        assert_eq!(jni_get_direct_buffer_address(env, buf), addr);
        assert_eq!(
            jni_get_direct_buffer_capacity(env, buf),
            0,
            "a zero capacity must read back as 0, not -1"
        );
    }

    /// **`GetDirectBufferAddress` refuses an arena block that cannot cover the
    /// capacity `GetDirectBufferCapacity` advertises for the same buffer.**
    ///
    /// The two entry points ARE the bound: the JNI contract says a native may
    /// touch `capacity` bytes from the address, so publishing them separately
    /// without checking they agree hands out a pointer with a bound nobody
    /// enforced. `unsafe_arena_real_ptr` has always known how many bytes were
    /// left in the block and this call site used to discard it -- recorded as
    /// "the handle TRANSLATED, and the bound dropped" in
    /// `zgc-rewrite-pass-walks-off-a-reference-array-20260815`.
    ///
    /// The accepting half is asserted first and on the SAME block: a guard that
    /// refuses every tagged handle would pass a refusal-only test while
    /// breaking every direct buffer netty and lz4-java hand to C.
    #[test]
    fn jni_direct_buffer_address_refuses_a_block_shorter_than_its_capacity() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();

        let handle = cratonvm_native_builtins::unsafe_arena_allocate(64);
        assert!(
            cratonvm_native_builtins::unsafe_arena_addr_is_tagged(handle),
            "the arena must hand back a tagged handle, or this test proves nothing"
        );
        let as_ptr = handle as usize as *mut u8;

        // Exactly covered: the pointer is published, and it is the REAL
        // address, not the handle -- returning the handle is the SIGSEGV in
        // third-party C that the translation exists to prevent.
        let ok = jni_new_direct_byte_buffer(env, as_ptr, 64);
        assert_ne!(ok, 0);
        let published = jni_get_direct_buffer_address(env, ok);
        assert!(
            !published.is_null(),
            "a block that covers its capacity must publish"
        );
        assert!(
            !cratonvm_native_builtins::unsafe_arena_addr_is_tagged(published as i64),
            "the published address must be translated, not the handle again"
        );
        assert_eq!(jni_get_direct_buffer_capacity(env, ok), 64);

        // One byte more than the block holds. That byte is the bug: a native
        // following the contract writes it, and it lands outside the block.
        let short = jni_new_direct_byte_buffer(env, as_ptr, 65);
        assert_ne!(short, 0);
        assert_eq!(
            jni_get_direct_buffer_capacity(env, short),
            65,
            "the capacity getter still answers what the object says"
        );
        assert!(
            jni_get_direct_buffer_address(env, short).is_null(),
            "and the address getter must refuse, because the pair would be a lie"
        );

        // An interior handle is bounded from its offset, so the same block
        // refuses a capacity it accepted from the base.
        let mid = jni_new_direct_byte_buffer(env, (handle + 32) as usize as *mut u8, 32);
        assert!(!jni_get_direct_buffer_address(env, mid).is_null());
        let mid_over = jni_new_direct_byte_buffer(env, (handle + 32) as usize as *mut u8, 33);
        assert!(jni_get_direct_buffer_address(env, mid_over).is_null());

        cratonvm_native_builtins::unsafe_arena_free(handle);
    }

    #[test]
    fn jni_direct_buffer_null_returns_null() {
        let env = get_jni_env();
        let buf = jni_new_direct_byte_buffer(env, std::ptr::null_mut(), 100);
        assert_eq!(
            buf, 0,
            "NewDirectByteBuffer with null address must return 0"
        );
        let addr = jni_get_direct_buffer_address(env, 0);
        assert!(
            addr.is_null(),
            "GetDirectBufferAddress(null) must return null"
        );
        let cap = jni_get_direct_buffer_capacity(env, 0);
        assert_eq!(cap, -1, "GetDirectBufferCapacity(null) must return -1");
    }

    #[test]
    fn jni_direct_buffer_negative_capacity() {
        let env = get_jni_env();
        let mut buf = [0u8; 16];
        let result = jni_new_direct_byte_buffer(env, buf.as_mut_ptr(), -1);
        assert_eq!(
            result, 0,
            "NewDirectByteBuffer with negative capacity must return 0"
        );
    }

    #[test]
    fn jni_get_module_returns_null() {
        let env = get_jni_env();
        let module = jni_get_module(env, 42); // some fake class
        assert_eq!(module, 0, "GetModule must return null (unnamed module)");
    }

    #[test]
    fn jni_v_variant_slots_not_stub() {
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        // NewObjectV (29)
        assert_ne!(unsafe { *(*env).add(29) }, stub_ptr, "slot 29 (NewObjectV)");
        // CallObjectMethodV (35)
        assert_ne!(
            unsafe { *(*env).add(35) },
            stub_ptr,
            "slot 35 (CallObjectMethodV)"
        );
        // CallIntMethodV (50)
        assert_ne!(
            unsafe { *(*env).add(50) },
            stub_ptr,
            "slot 50 (CallIntMethodV)"
        );
        // CallVoidMethodV (62)
        assert_ne!(
            unsafe { *(*env).add(62) },
            stub_ptr,
            "slot 62 (CallVoidMethodV)"
        );
        // CallNonvirtualObjectMethodV (65)
        assert_ne!(
            unsafe { *(*env).add(65) },
            stub_ptr,
            "slot 65 (CallNonvirtualObjectMethodV)"
        );
        // CallNonvirtualVoidMethodV (92)
        assert_ne!(
            unsafe { *(*env).add(92) },
            stub_ptr,
            "slot 92 (CallNonvirtualVoidMethodV)"
        );
        // CallStaticObjectMethodV (115)
        assert_ne!(
            unsafe { *(*env).add(115) },
            stub_ptr,
            "slot 115 (CallStaticObjectMethodV)"
        );
        // CallStaticIntMethodV (130)
        assert_ne!(
            unsafe { *(*env).add(130) },
            stub_ptr,
            "slot 130 (CallStaticIntMethodV)"
        );
        // CallStaticVoidMethodV (142)
        assert_ne!(
            unsafe { *(*env).add(142) },
            stub_ptr,
            "slot 142 (CallStaticVoidMethodV)"
        );
    }

    #[test]
    fn jni_nio_slots_not_stub() {
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(
            unsafe { *(*env).add(229) },
            stub_ptr,
            "slot 229 (NewDirectByteBuffer)"
        );
        assert_ne!(
            unsafe { *(*env).add(230) },
            stub_ptr,
            "slot 230 (GetDirectBufferAddress)"
        );
        assert_ne!(
            unsafe { *(*env).add(231) },
            stub_ptr,
            "slot 231 (GetDirectBufferCapacity)"
        );
        assert_ne!(
            unsafe { *(*env).add(232) },
            stub_ptr,
            "slot 232 (GetObjectRefType)"
        );
        // Slot 233 is checked by NAME, not by "is not the stub". `GetModule`
        // returns a constant 0 and so is foldable with `jni_stub` by the
        // linker — see `jni_function_table_extended_to_234` for the full
        // reasoning and the measurement. The four slots above have bodies that
        // do real work, so nothing folds them and `assert_ne!` remains a
        // meaningful question for them.
        assert_eq!(
            unsafe { *(*env).add(233) },
            jni_get_module as *const () as usize,
            "slot 233 (GetModule)"
        );
    }

    /// A `jclass` must never be 0, because in JNI a NULL `jobject` means
    /// FAILURE.
    ///
    /// The encoding was the bare `ClassId`, so `ClassId(0)` — the FIRST class
    /// the VM loads, i.e. `java.lang.Object` — was unreachable through
    /// `FindClass`: the call returned 0 and every native library read it as
    /// "no such class". Measured with a C probe:
    /// `FindClass(java/lang/Object) = (nil)` from both `JNI_OnLoad` and a
    /// registered native, against a live handle on HotSpot.
    ///
    /// The tag also has to stay TRANSPARENT to the twenty-one decode sites that
    /// read a `jclass` as `ClassId::new(clazz as u32)`.
    #[test]
    fn jclass_encoding_is_never_null_and_survives_the_u32_decode() {
        for raw in [0u32, 1, 2, 255, 0x7fff_ffff, u32::MAX] {
            let handle = class_id_to_jclass(ClassId::new(raw));
            assert_ne!(
                handle, 0,
                "ClassId({raw}) encoded to a NULL jclass — FindClass would report \
                 'no such class' for it"
            );
            assert_eq!(
                handle as u32, raw,
                "the tag must vanish under the `clazz as u32` decode every \
                 jclass consumer uses"
            );
            assert!(is_jclass_handle(handle), "the tag must be present");
            assert!(
                handle > 0x0000_8000_0000_0000,
                "the tag must sit above every canonical user-space address, so a \
                 jclass can never be confused with a heap pointer"
            );
        }
        // Distinct classes must stay distinct handles.
        assert_ne!(
            class_id_to_jclass(ClassId::new(0)),
            class_id_to_jclass(ClassId::new(1))
        );
    }

    /// Round 12 wave 1 (lane rt), `w36-jni-native-returning-a-jclass-hands-java-null`:
    /// a `jclass` read back as an OBJECT (a native's `Class` return, an array
    /// store) is the class's mirror, not `null`, for even and odd `ClassId`s.
    #[test]
    fn a_jclass_read_as_an_object_is_its_class_mirror() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        thread_local! {
            static RETURNED: Cell<JObject> = const { Cell::new(0) };
        }
        extern "C" fn returns_a_jclass(_env: JNIEnv, _cls: JObject) -> u64 {
            RETURNED.with(Cell::get)
        }
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let env = get_jni_env();
        let text = CString::new("r12 jclass mirror").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        assert_ne!(s, 0);
        let string_class = jni_get_object_class(env, s);
        assert!(is_jclass_handle(string_class));
        let class_id = jclass_class_id(string_class);
        let mirror = jobject_to_obj(string_class).expect("a jclass names its mirror");
        assert_eq!(crate::vm::class_id_from_mirror(&shared, mirror), Some(class_id));
        assert_eq!(jobject_to_obj(string_class), Some(mirror), "one mirror per class");

        // `static native Class<?> f()` returning the jclass it looked up.
        RETURNED.with(|c| c.set(string_class));
        let r = unsafe {
            dispatch_jni_native(
                returns_a_jclass as *const () as usize,
                env,
                0,
                &[],
                "()Ljava/lang/Class;",
            )
        };
        assert_eq!(r, Value::Object(Some(mirror)), "the native's jclass return");

        // An odd `ClassId` took the global-ref branch before the fix.
        let odd = (1..512u32)
            .step_by(2)
            .map(ClassId::new)
            .find(|id| shared.classes.class_manager.read().get_class(*id).is_some());
        if let Some(odd) = odd {
            let m = jobject_to_obj(class_id_to_jclass(odd)).expect("an odd ClassId resolves");
            assert_eq!(crate::vm::class_id_from_mirror(&shared, m), Some(odd));
        }

        // Stored into an array and read back: the same class.
        let class_class = jni_get_object_class(env, string_class);
        let arr = jni_new_object_array(env, 1, class_class, 0);
        assert_ne!(arr, 0);
        jni_set_object_array_element(env, arr, 0, string_class);
        let back = jni_get_object_array_element(env, arr, 0);
        assert_ne!(back, 0, "SetObjectArrayElement stored null");
        assert_eq!(jclass_class_id(back), class_id);
        assert_eq!(jni_is_same_object(env, back, string_class), JNI_TRUE);

        // An index outside the array leaves an exception pending (HotSpot:
        // `ArrayIndexOutOfBoundsException`) instead of a silent NULL / no-op.
        // No `JvmThread` here, so what is left is the `ThrowNew` sentinel.
        for bad in [-1, 1, 7] {
            JNI_PENDING_EXCEPTION.with(|c| c.set(0));
            assert_eq!(jni_get_object_array_element(env, arr, bad), 0);
            assert_ne!(JNI_PENDING_EXCEPTION.with(|c| c.get()), 0, "Get index {bad}");
            JNI_PENDING_EXCEPTION.with(|c| c.set(0));
            jni_set_object_array_element(env, arr, bad, string_class);
            assert_ne!(JNI_PENDING_EXCEPTION.with(|c| c.get()), 0, "Set index {bad}");
        }
        JNI_PENDING_EXCEPTION.with(|c| c.set(0));
        assert_ne!(jni_get_object_array_element(env, arr, 0), 0, "index 0 is in bounds");
        assert_eq!(JNI_PENDING_EXCEPTION.with(|c| c.get()), 0);

        // A negative length leaves `NegativeArraySizeException` pending.
        assert_eq!(jni_new_int_array(env, -1), 0);
        assert_ne!(JNI_PENDING_EXCEPTION.with(|c| c.get()), 0, "NewIntArray(-1)");
        JNI_PENDING_EXCEPTION.with(|c| c.set(0));
        assert_eq!(jni_new_object_array(env, -2, class_class, 0), 0);
        assert_ne!(JNI_PENDING_EXCEPTION.with(|c| c.get()), 0, "NewObjectArray(-2)");
        JNI_PENDING_EXCEPTION.with(|c| c.set(0));

        // A tag naming no class stays NULL; `Throw(cls)` is still refused.
        assert_eq!(jobject_to_obj(class_id_to_jclass(ClassId::new(0x7fff_fff1))), None);
        assert_eq!(jni_throw(env, string_class), JNI_ERR);
    }

    #[test]
    fn jni_get_object_ref_type_at_slot_232() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(232) };
        let get_ref_type: extern "C" fn(JNIEnv, JObject) -> JInt =
            unsafe { std::mem::transmute(func_ptr) };
        // null → JNIInvalidRefType = 0
        assert_eq!(get_ref_type(env, 0), 0);
    }

    #[test]
    fn jni_count_non_stub_functions() {
        // Verify that we have at least 165 non-stub functions (was ~129, now ~165+).
        //
        // This count is a LOWER BOUND and can legitimately read lower in
        // release than in debug: any wired function whose body is identical to
        // `jni_stub` (returns a constant 0, ignores its arguments) may be
        // folded onto the stub's address by `/OPT:ICF` and counted here as a
        // stub. `GetModule` is exactly such a function — see
        // `jni_function_table_extended_to_234`. The assertion is `>=`, so
        // folding can only make it stricter, never falsely green; if it ever
        // trips, check whether a slot was un-wired before assuming folding.
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        let mut non_stub_count = 0;
        for i in 0..JNI_FUNCTION_COUNT {
            let ptr = unsafe { *(*env).add(i) };
            if ptr != stub_ptr {
                non_stub_count += 1;
            }
        }
        assert!(
            non_stub_count >= 165,
            "Expected at least 165 non-stub JNI functions, got {non_stub_count}"
        );
    }

    #[test]
    fn jni_va_list_to_jvalues_null_mid() {
        // null method ID → returns null pointer
        let (ptr, len) = va_list_to_jvalues(0, std::ptr::null_mut());
        assert!(ptr.is_null());
        assert_eq!(len, 0);
    }

    #[test]
    fn jni_invoke_table_attach_detach() {
        let vm = get_java_vm();
        // AttachCurrentThread (slot 4)
        let func_ptr = unsafe { *(*vm).add(4) };
        assert_ne!(func_ptr, 0, "AttachCurrentThread must be registered");
        // DetachCurrentThread (slot 5)
        let func_ptr = unsafe { *(*vm).add(5) };
        assert_ne!(func_ptr, 0, "DetachCurrentThread must be registered");
        // AttachCurrentThreadAsDaemon (slot 7)
        let func_ptr = unsafe { *(*vm).add(7) };
        assert_ne!(
            func_ptr, 0,
            "AttachCurrentThreadAsDaemon must be registered"
        );
    }

    // ---- Modified UTF-8 encoding (GetStringUTFChars contract) ----

    #[test]
    fn modified_utf8_plain_ascii_unchanged() {
        assert_eq!(to_modified_utf8("hello"), b"hello".to_vec());
    }

    #[test]
    fn modified_utf8_interior_nul_is_two_bytes() {
        // Interior U+0000 must encode as 0xC0 0x80, never a 0x00 byte.
        let s = "a\u{0}b";
        let m = to_modified_utf8(s);
        assert_eq!(m, vec![b'a', 0xC0, 0x80, b'b']);
        // The result contains no interior NUL, so CString::new succeeds.
        assert!(std::ffi::CString::new(m).is_ok());
    }

    #[test]
    fn modified_utf8_two_byte_char() {
        // U+00E9 (é) → 0xC3 0xA9 (same as standard UTF-8 for the BMP <= 0x7FF).
        assert_eq!(to_modified_utf8("\u{00e9}"), vec![0xC3, 0xA9]);
    }

    #[test]
    fn modified_utf8_three_byte_bmp() {
        // U+20AC (€) → 0xE2 0x82 0xAC.
        assert_eq!(to_modified_utf8("\u{20ac}"), vec![0xE2, 0x82, 0xAC]);
    }

    #[test]
    fn modified_utf8_supplementary_is_surrogate_pair() {
        // U+1F600 (😀) → CESU-8 surrogate pair, two 3-byte sequences.
        // High surrogate D83D → ED A0 BD; low surrogate DE00 → ED B8 80.
        let m = to_modified_utf8("\u{1f600}");
        assert_eq!(m, vec![0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]);
        // No interior NUL bytes anywhere.
        assert!(!m.contains(&0));
    }

    #[test]
    fn modified_utf8_len_agrees_with_encoder() {
        // GetStringUTFLength must report exactly the byte count GetStringUTFChars
        // produces (excluding the trailing NUL) for every code-point class.
        for s in [
            "",
            "ascii",
            "a\u{0}b",
            "\u{00e9}",
            "\u{20ac}",
            "\u{1f600}",
            "mix\u{0}é€😀",
        ] {
            assert_eq!(
                modified_utf8_len(s),
                to_modified_utf8(s).len(),
                "length mismatch for {s:?}"
            );
        }
    }

    /// `GetStringUTFRegion` must write **modified** UTF-8, the same encoding
    /// `GetStringUTFChars` writes and `GetStringUTFLength` measures.
    ///
    /// It used to write `region.as_bytes()` — standard UTF-8 — while both
    /// siblings went through `to_modified_utf8` / `modified_utf8_len`. The
    /// assertions below are the two ways that disagreement is observable to a
    /// native caller; the source witness after them pins the call site, since
    /// driving the real entry point needs a live `SharedVm` and a heap string.
    #[test]
    fn get_string_utf_region_writes_modified_utf8_not_standard() {
        // Hazard 1 — interior NUL. Standard UTF-8 emits a bare 0x00, which
        // terminates the buffer for every C consumer: "a\0b" reads back "a".
        let nul = "a\u{0}b";
        assert_eq!(nul.as_bytes(), &[0x61, 0x00, 0x62]);
        assert_eq!(to_modified_utf8(nul), vec![0x61, 0xC0, 0x80, 0x62]);
        assert!(
            !to_modified_utf8(nul).contains(&0),
            "modified UTF-8 must contain no interior NUL"
        );

        // Hazard 2 — a supplementary character. `GetStringUTFLength` reports 6
        // (the CESU-8 surrogate pair), so the canonical caller mallocs 6 and
        // reads 6. Standard UTF-8 writes only 4, leaving two bytes of the
        // caller's buffer uninitialized.
        let emoji = "\u{1f600}";
        assert_eq!(emoji.len(), 4, "standard UTF-8 is 4 bytes");
        assert_eq!(
            modified_utf8_len(emoji),
            6,
            "GetStringUTFLength promises 6 bytes for this region"
        );
        assert_eq!(to_modified_utf8(emoji).len(), 6);

        // For every string, what the region path now writes must be exactly
        // what GetStringUTFLength told the caller to expect.
        for s in [
            "",
            "ascii",
            nul,
            "\u{00e9}",
            "\u{20ac}",
            emoji,
            "mix\u{0}é€😀",
        ] {
            assert_eq!(
                to_modified_utf8(s).len(),
                modified_utf8_len(s),
                "region byte count must match the advertised UTF length for {s:?}"
            );
        }

        // Source witness: the entry point must use the shared encoder.
        let src =
            std::fs::read_to_string(format!("{}/src/native/jni.rs", env!("CARGO_MANIFEST_DIR")))
                .expect("read jni.rs");
        let start = src
            .find("extern \"C\" fn jni_get_string_utf_region(")
            .expect("jni_get_string_utf_region must still exist");
        let end = start
            + src[start..]
                .find("\n}\n")
                .expect("function body must terminate");
        let body = &src[start..end];
        assert!(
            body.contains("to_modified_utf8(&region)"),
            "GetStringUTFRegion must encode through `to_modified_utf8`, the same \
             encoder GetStringUTFChars uses and GetStringUTFLength measures"
        );
        assert!(
            !body.contains("region.as_bytes()"),
            "GetStringUTFRegion must not fall back to standard UTF-8 \
             (`region.as_bytes()`): it truncates at an interior NUL and \
             under-writes a caller buffer sized by GetStringUTFLength"
        );
    }

    // ---- JNI argument register classification (ABI marshalling) ----

    #[test]
    fn jni_arg_register_class() {
        assert!(!JniArg::int(42).is_fp);
        assert!(JniArg::float(1.5f64.to_bits()).is_fp);
        assert_eq!(JniArg::int(0xDEAD).bits, 0xDEAD);
    }

    // ---- ABI marshalling end-to-end (x86_64 trampoline) ----
    //
    // These exercise the FP-register and stack-spill paths the integer
    // fast-path cannot express. They run real machine code through the naked
    // trampoline, so a passing run validates register placement on the host ABI.

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn dispatch_marshals_double_arg_and_return() {
        // double f(env, this, double a, double b) { return a*10 + b; }
        extern "C" fn mul_add(_env: JNIEnv, _this: JObject, a: f64, b: f64) -> f64 {
            a * 10.0 + b
        }
        let fn_ptr = mul_add as *const () as usize;
        let env = get_jni_env();
        let args = [
            crate::types::Value::Double(3.5),
            crate::types::Value::Double(0.25),
        ];
        let result = unsafe { dispatch_jni_native(fn_ptr, env, 0, &args, "(DD)D") };
        assert_eq!(result, crate::types::Value::Double(35.25));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn dispatch_marshals_mixed_int_float_args() {
        // int f(env, this, int i, float fl, long l) { return i + (int)fl + (int)l; }
        extern "C" fn mixed(_env: JNIEnv, _this: JObject, i: i32, fl: f32, l: i64) -> i32 {
            i + fl as i32 + l as i32
        }
        let fn_ptr = mixed as *const () as usize;
        let env = get_jni_env();
        let args = [
            crate::types::Value::Int(100),
            crate::types::Value::Float(20.0),
            crate::types::Value::Long(3),
        ];
        let result = unsafe { dispatch_jni_native(fn_ptr, env, 0, &args, "(IFJ)I") };
        assert_eq!(result, crate::types::Value::Int(123));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn dispatch_spills_excess_integer_args_to_stack() {
        // Ten integer Java args (+ env + this = 12 total) — forces stack spilling
        // on both Win64 (4 GP regs) and SysV (6 GP regs).
        #[allow(clippy::too_many_arguments)]
        extern "C" fn sum10(
            _env: JNIEnv,
            _this: JObject,
            a: i64,
            b: i64,
            c: i64,
            d: i64,
            e: i64,
            f: i64,
            g: i64,
            h: i64,
            i: i64,
            j: i64,
        ) -> i64 {
            a + b + c + d + e + f + g + h + i + j
        }
        let fn_ptr = sum10 as *const () as usize;
        let env = get_jni_env();
        let args: Vec<crate::types::Value> = (1..=10)
            .map(|n| crate::types::Value::Long(n as i64))
            .collect();
        let result = unsafe { dispatch_jni_native(fn_ptr, env, 0, &args, "(JJJJJJJJJJ)J") };
        assert_eq!(result, crate::types::Value::Long(55));
    }

    // -----------------------------------------------------------------------
    // The JNI function table's LAYOUT
    // -----------------------------------------------------------------------

    /// Normalise a name to letters and digits, lower-cased, so
    /// `GetStringUTFChars` and `get_string_utf_chars` compare equal.
    #[cfg(target_arch = "x86_64")]
    fn squash(name: &str) -> String {
        name.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }

    /// Every slot of `JNINativeInterface_`, in `jni.h` order, starting at the
    /// first non-reserved slot (4, `GetVersion`) and ending at the last slot
    /// JDK 25 declares (235, `GetStringUTFLengthAsLong`).
    ///
    /// A native never NAMES a JNI function: it loads slot N from the table and
    /// calls it. So the only thing that makes `(*env)->DeleteLocalRef(...)`
    /// mean "delete a local ref" is that slot 23 holds this VM's
    /// `jni_delete_local_ref` — and until 2026-08-06 it did not. A second
    /// `GetObjectRefType` had been wired at 19, which pushed `PushLocalFrame`
    /// through `AllocObject` one slot along: `DeleteLocalRef` ran
    /// `DeleteGlobalRef`, `NewGlobalRef` ran `PopLocalFrame`,
    /// `DeleteGlobalRef` ran `NewGlobalRef` (so every release leaked a global
    /// root), and `IsSameObject` — the idiom every library uses for a null
    /// check — ran the `void` `DeleteLocalRef` and returned stack residue.
    ///
    /// Every test that existed then was written against the slot the table
    /// HAPPENED to use rather than the slot the header specifies, so all of
    /// them stayed green through it. This one is written the other way round:
    /// the left column is the header, verbatim, and position IS the index.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn jni_function_table_matches_the_jni_h_layout() {
        // Each row: the name jni.h gives the slot, the function this VM
        // wires there. Position IS the index — the first row is slot 4.
        macro_rules! slots {
            ($($name:literal => $f:path),* $(,)?) => {
                &[$( ($name, $f as *const () as usize, stringify!($f)) ),*]
            };
        }
        let header: &[(&str, usize, &str)] = slots![
            "GetVersion" => jni_get_version,
            "DefineClass" => jni_define_class,
            "FindClass" => jni_find_class,
            "FromReflectedMethod" => jni_from_reflected_method,
            "FromReflectedField" => jni_from_reflected_field,
            "ToReflectedMethod" => jni_to_reflected_method,
            "GetSuperclass" => jni_get_superclass,
            "IsAssignableFrom" => jni_is_assignable_from,
            "ToReflectedField" => jni_to_reflected_field,
            "Throw" => jni_throw,
            "ThrowNew" => jni_throw_new,
            "ExceptionOccurred" => jni_exception_occurred,
            "ExceptionDescribe" => jni_exception_describe,
            "ExceptionClear" => jni_exception_clear,
            "FatalError" => jni_fatal_error,
            "PushLocalFrame" => jni_push_local_frame,
            "PopLocalFrame" => jni_pop_local_frame,
            "NewGlobalRef" => jni_new_global_ref,
            "DeleteGlobalRef" => jni_delete_global_ref,
            "DeleteLocalRef" => jni_delete_local_ref,
            "IsSameObject" => jni_is_same_object,
            "NewLocalRef" => jni_new_local_ref,
            "EnsureLocalCapacity" => jni_ensure_local_capacity,
            "AllocObject" => jni_alloc_object,
            "NewObject" => jni_va_new_object,
            "NewObjectV" => jni_new_object_v,
            "NewObjectA" => jni_new_object_a,
            "GetObjectClass" => jni_get_object_class,
            "IsInstanceOf" => jni_is_instance_of,
            "GetMethodID" => jni_get_method_id,
            "CallObjectMethod" => jni_va_call_object_method,
            "CallObjectMethodV" => jni_call_object_method_v,
            "CallObjectMethodA" => jni_call_object_method_a,
            "CallBooleanMethod" => jni_va_call_boolean_method,
            "CallBooleanMethodV" => jni_call_boolean_method_v,
            "CallBooleanMethodA" => jni_call_boolean_method_a,
            "CallByteMethod" => jni_va_call_byte_method,
            "CallByteMethodV" => jni_call_byte_method_v,
            "CallByteMethodA" => jni_call_byte_method_a,
            "CallCharMethod" => jni_va_call_char_method,
            "CallCharMethodV" => jni_call_char_method_v,
            "CallCharMethodA" => jni_call_char_method_a,
            "CallShortMethod" => jni_va_call_short_method,
            "CallShortMethodV" => jni_call_short_method_v,
            "CallShortMethodA" => jni_call_short_method_a,
            "CallIntMethod" => jni_va_call_int_method,
            "CallIntMethodV" => jni_call_int_method_v,
            "CallIntMethodA" => jni_call_int_method_a,
            "CallLongMethod" => jni_va_call_long_method,
            "CallLongMethodV" => jni_call_long_method_v,
            "CallLongMethodA" => jni_call_long_method_a,
            "CallFloatMethod" => jni_va_call_float_method,
            "CallFloatMethodV" => jni_call_float_method_v,
            "CallFloatMethodA" => jni_call_float_method_a,
            "CallDoubleMethod" => jni_va_call_double_method,
            "CallDoubleMethodV" => jni_call_double_method_v,
            "CallDoubleMethodA" => jni_call_double_method_a,
            "CallVoidMethod" => jni_va_call_void_method,
            "CallVoidMethodV" => jni_call_void_method_v,
            "CallVoidMethodA" => jni_call_void_method_a,
            "CallNonvirtualObjectMethod" => jni_va_call_nonvirtual_object_method,
            "CallNonvirtualObjectMethodV" => jni_call_nonvirtual_object_method_v,
            "CallNonvirtualObjectMethodA" => jni_call_nonvirtual_object_method_a,
            "CallNonvirtualBooleanMethod" => jni_va_call_nonvirtual_boolean_method,
            "CallNonvirtualBooleanMethodV" => jni_call_nonvirtual_boolean_method_v,
            "CallNonvirtualBooleanMethodA" => jni_call_nonvirtual_boolean_method_a,
            "CallNonvirtualByteMethod" => jni_va_call_nonvirtual_byte_method,
            "CallNonvirtualByteMethodV" => jni_call_nonvirtual_byte_method_v,
            "CallNonvirtualByteMethodA" => jni_call_nonvirtual_byte_method_a,
            "CallNonvirtualCharMethod" => jni_va_call_nonvirtual_char_method,
            "CallNonvirtualCharMethodV" => jni_call_nonvirtual_char_method_v,
            "CallNonvirtualCharMethodA" => jni_call_nonvirtual_char_method_a,
            "CallNonvirtualShortMethod" => jni_va_call_nonvirtual_short_method,
            "CallNonvirtualShortMethodV" => jni_call_nonvirtual_short_method_v,
            "CallNonvirtualShortMethodA" => jni_call_nonvirtual_short_method_a,
            "CallNonvirtualIntMethod" => jni_va_call_nonvirtual_int_method,
            "CallNonvirtualIntMethodV" => jni_call_nonvirtual_int_method_v,
            "CallNonvirtualIntMethodA" => jni_call_nonvirtual_int_method_a,
            "CallNonvirtualLongMethod" => jni_va_call_nonvirtual_long_method,
            "CallNonvirtualLongMethodV" => jni_call_nonvirtual_long_method_v,
            "CallNonvirtualLongMethodA" => jni_call_nonvirtual_long_method_a,
            "CallNonvirtualFloatMethod" => jni_va_call_nonvirtual_float_method,
            "CallNonvirtualFloatMethodV" => jni_call_nonvirtual_float_method_v,
            "CallNonvirtualFloatMethodA" => jni_call_nonvirtual_float_method_a,
            "CallNonvirtualDoubleMethod" => jni_va_call_nonvirtual_double_method,
            "CallNonvirtualDoubleMethodV" => jni_call_nonvirtual_double_method_v,
            "CallNonvirtualDoubleMethodA" => jni_call_nonvirtual_double_method_a,
            "CallNonvirtualVoidMethod" => jni_va_call_nonvirtual_void_method,
            "CallNonvirtualVoidMethodV" => jni_call_nonvirtual_void_method_v,
            "CallNonvirtualVoidMethodA" => jni_call_nonvirtual_void_method_a,
            "GetFieldID" => jni_get_field_id,
            "GetObjectField" => jni_get_object_field,
            "GetBooleanField" => jni_get_boolean_field,
            "GetByteField" => jni_get_byte_field,
            "GetCharField" => jni_get_char_field,
            "GetShortField" => jni_get_short_field,
            "GetIntField" => jni_get_int_field,
            "GetLongField" => jni_get_long_field,
            "GetFloatField" => jni_get_float_field,
            "GetDoubleField" => jni_get_double_field,
            "SetObjectField" => jni_set_object_field,
            "SetBooleanField" => jni_set_boolean_field,
            "SetByteField" => jni_set_byte_field,
            "SetCharField" => jni_set_char_field,
            "SetShortField" => jni_set_short_field,
            "SetIntField" => jni_set_int_field,
            "SetLongField" => jni_set_long_field,
            "SetFloatField" => jni_set_float_field,
            "SetDoubleField" => jni_set_double_field,
            "GetStaticMethodID" => jni_get_static_method_id,
            "CallStaticObjectMethod" => jni_va_call_static_object_method,
            "CallStaticObjectMethodV" => jni_call_static_object_method_v,
            "CallStaticObjectMethodA" => jni_call_static_object_method_a,
            "CallStaticBooleanMethod" => jni_va_call_static_boolean_method,
            "CallStaticBooleanMethodV" => jni_call_static_boolean_method_v,
            "CallStaticBooleanMethodA" => jni_call_static_boolean_method_a,
            "CallStaticByteMethod" => jni_va_call_static_byte_method,
            "CallStaticByteMethodV" => jni_call_static_byte_method_v,
            "CallStaticByteMethodA" => jni_call_static_byte_method_a,
            "CallStaticCharMethod" => jni_va_call_static_char_method,
            "CallStaticCharMethodV" => jni_call_static_char_method_v,
            "CallStaticCharMethodA" => jni_call_static_char_method_a,
            "CallStaticShortMethod" => jni_va_call_static_short_method,
            "CallStaticShortMethodV" => jni_call_static_short_method_v,
            "CallStaticShortMethodA" => jni_call_static_short_method_a,
            "CallStaticIntMethod" => jni_va_call_static_int_method,
            "CallStaticIntMethodV" => jni_call_static_int_method_v,
            "CallStaticIntMethodA" => jni_call_static_int_method_a,
            "CallStaticLongMethod" => jni_va_call_static_long_method,
            "CallStaticLongMethodV" => jni_call_static_long_method_v,
            "CallStaticLongMethodA" => jni_call_static_long_method_a,
            "CallStaticFloatMethod" => jni_va_call_static_float_method,
            "CallStaticFloatMethodV" => jni_call_static_float_method_v,
            "CallStaticFloatMethodA" => jni_call_static_float_method_a,
            "CallStaticDoubleMethod" => jni_va_call_static_double_method,
            "CallStaticDoubleMethodV" => jni_call_static_double_method_v,
            "CallStaticDoubleMethodA" => jni_call_static_double_method_a,
            "CallStaticVoidMethod" => jni_va_call_static_void_method,
            "CallStaticVoidMethodV" => jni_call_static_void_method_v,
            "CallStaticVoidMethodA" => jni_call_static_void_method_a,
            "GetStaticFieldID" => jni_get_static_field_id,
            "GetStaticObjectField" => jni_get_static_object_field,
            "GetStaticBooleanField" => jni_get_static_boolean_field,
            "GetStaticByteField" => jni_get_static_byte_field,
            "GetStaticCharField" => jni_get_static_char_field,
            "GetStaticShortField" => jni_get_static_short_field,
            "GetStaticIntField" => jni_get_static_int_field,
            "GetStaticLongField" => jni_get_static_long_field,
            "GetStaticFloatField" => jni_get_static_float_field,
            "GetStaticDoubleField" => jni_get_static_double_field,
            "SetStaticObjectField" => jni_set_static_object_field,
            "SetStaticBooleanField" => jni_set_static_boolean_field,
            "SetStaticByteField" => jni_set_static_byte_field,
            "SetStaticCharField" => jni_set_static_char_field,
            "SetStaticShortField" => jni_set_static_short_field,
            "SetStaticIntField" => jni_set_static_int_field,
            "SetStaticLongField" => jni_set_static_long_field,
            "SetStaticFloatField" => jni_set_static_float_field,
            "SetStaticDoubleField" => jni_set_static_double_field,
            "NewString" => jni_new_string,
            "GetStringLength" => jni_get_string_length,
            "GetStringChars" => jni_get_string_chars,
            "ReleaseStringChars" => jni_release_string_chars,
            "NewStringUTF" => jni_new_string_utf,
            "GetStringUTFLength" => jni_get_string_utf_length,
            "GetStringUTFChars" => jni_get_string_utf_chars,
            "ReleaseStringUTFChars" => jni_release_string_utf_chars,
            "GetArrayLength" => jni_get_array_length,
            "NewObjectArray" => jni_new_object_array,
            "GetObjectArrayElement" => jni_get_object_array_element,
            "SetObjectArrayElement" => jni_set_object_array_element,
            "NewBooleanArray" => jni_new_boolean_array,
            "NewByteArray" => jni_new_byte_array,
            "NewCharArray" => jni_new_char_array,
            "NewShortArray" => jni_new_short_array,
            "NewIntArray" => jni_new_int_array,
            "NewLongArray" => jni_new_long_array,
            "NewFloatArray" => jni_new_float_array,
            "NewDoubleArray" => jni_new_double_array,
            "GetBooleanArrayElements" => jni_get_boolean_array_elements,
            "GetByteArrayElements" => jni_get_byte_array_elements,
            "GetCharArrayElements" => jni_get_char_array_elements,
            "GetShortArrayElements" => jni_get_short_array_elements,
            "GetIntArrayElements" => jni_get_int_array_elements,
            "GetLongArrayElements" => jni_get_long_array_elements,
            "GetFloatArrayElements" => jni_get_float_array_elements,
            "GetDoubleArrayElements" => jni_get_double_array_elements,
            "ReleaseBooleanArrayElements" => jni_release_boolean_array_elements,
            "ReleaseByteArrayElements" => jni_release_byte_array_elements,
            "ReleaseCharArrayElements" => jni_release_char_array_elements,
            "ReleaseShortArrayElements" => jni_release_short_array_elements,
            "ReleaseIntArrayElements" => jni_release_int_array_elements,
            "ReleaseLongArrayElements" => jni_release_long_array_elements,
            "ReleaseFloatArrayElements" => jni_release_float_array_elements,
            "ReleaseDoubleArrayElements" => jni_release_double_array_elements,
            "GetBooleanArrayRegion" => jni_get_boolean_array_region,
            "GetByteArrayRegion" => jni_get_byte_array_region,
            "GetCharArrayRegion" => jni_get_char_array_region,
            "GetShortArrayRegion" => jni_get_short_array_region,
            "GetIntArrayRegion" => jni_get_int_array_region,
            "GetLongArrayRegion" => jni_get_long_array_region,
            "GetFloatArrayRegion" => jni_get_float_array_region,
            "GetDoubleArrayRegion" => jni_get_double_array_region,
            "SetBooleanArrayRegion" => jni_set_boolean_array_region,
            "SetByteArrayRegion" => jni_set_byte_array_region,
            "SetCharArrayRegion" => jni_set_char_array_region,
            "SetShortArrayRegion" => jni_set_short_array_region,
            "SetIntArrayRegion" => jni_set_int_array_region,
            "SetLongArrayRegion" => jni_set_long_array_region,
            "SetFloatArrayRegion" => jni_set_float_array_region,
            "SetDoubleArrayRegion" => jni_set_double_array_region,
            "RegisterNatives" => jni_register_natives,
            "UnregisterNatives" => jni_unregister_natives,
            "MonitorEnter" => jni_monitor_enter,
            "MonitorExit" => jni_monitor_exit,
            "GetJavaVM" => jni_get_java_vm,
            "GetStringRegion" => jni_get_string_region,
            "GetStringUTFRegion" => jni_get_string_utf_region,
            "GetPrimitiveArrayCritical" => jni_get_primitive_array_critical,
            "ReleasePrimitiveArrayCritical" => jni_release_primitive_array_critical,
            "GetStringCritical" => jni_get_string_critical,
            "ReleaseStringCritical" => jni_release_string_critical,
            "NewWeakGlobalRef" => jni_new_weak_global_ref,
            "DeleteWeakGlobalRef" => jni_delete_weak_global_ref,
            "ExceptionCheck" => jni_exception_check,
            "NewDirectByteBuffer" => jni_new_direct_byte_buffer,
            "GetDirectBufferAddress" => jni_get_direct_buffer_address,
            "GetDirectBufferCapacity" => jni_get_direct_buffer_capacity,
            "GetObjectRefType" => jni_get_object_ref_type,
            "GetModule" => jni_get_module,
            "IsVirtualThread" => jni_is_virtual_thread,
            "GetStringUTFLengthAsLong" => jni_get_string_utf_length_as_long,
        ];
        assert_eq!(
            header.len(),
            JNI_FUNCTION_COUNT - 4,
            "the table must have exactly one slot per jni.h entry after the \
             four reserved ones"
        );
        let env = get_jni_env();
        for (i, (header_name, wired, rust_name)) in header.iter().enumerate() {
            let slot = i + 4;
            let actual = unsafe { *(*env).add(slot) };
            assert_eq!(
                actual, *wired,
                "slot {slot} must dispatch to `{rust_name}`, which is this \
                 VM's {header_name}"
            );
            // And the wired function's own name must BE the header's name.
            // That is what makes the table a check rather than a copy of the
            // wiring: a row can only satisfy both columns if the slot is right.
            let rust = rust_name
                .strip_prefix("jni_va_")
                .or_else(|| rust_name.strip_prefix("jni_"))
                .unwrap_or(rust_name);
            assert_eq!(
                squash(rust),
                squash(header_name),
                "slot {slot} is wired to `{rust_name}` but jni.h calls it \
                 `{header_name}`"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The function-pointer guard
    // -----------------------------------------------------------------------

    /// An odd entry address is legal x86-64 and must be CALLED, not refused.
    ///
    /// `cc -shared -fPIC -O1` on the strict corpus's own JNI fixture put
    /// `add` at `0x11f7`, `mulLong` at `0x11ff` and `scale` at `0x120b`. The
    /// old `fn_ptr % 2 != 0` guard refused all three and returned 0, so
    /// `add(40, 2)` read as `0` in Java — a wrong VALUE manufactured by a
    /// safety check, from a correctly resolved pointer into correctly
    /// compiled code.
    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn jni_fn_ptr_odd_address_is_callable() {
        assert!(!jni_fn_ptr_ok(0), "a null pointer is still refused");
        assert!(jni_fn_ptr_ok(0x11f7), "x86-64 has no entry-alignment rule");
        assert!(jni_fn_ptr_ok(0x120b));
    }

    // -----------------------------------------------------------------------
    // The bare-varargs trampolines
    // -----------------------------------------------------------------------

    // Two trampolines built by the SAME macro the 31 real ones use, pointed at
    // sinks that report what arrived. Calling them through a genuine variadic
    // function-pointer type makes the compiler emit a real C varargs call
    // sequence for the active ABI — including System V's `AL` = SSE-register
    // count — so this exercises the trampoline, not a hand-rolled imitation.
    #[cfg(target_arch = "x86_64")]
    jni_varargs_trampoline!(@n 3, jni_va_test_three_named, jni_va_test_sink3);
    #[cfg(target_arch = "x86_64")]
    jni_varargs_trampoline!(@n 4, jni_va_test_four_named, jni_va_test_sink4);
    #[cfg(target_arch = "x86_64")]
    extern "C" {
        fn jni_va_test_three_named();
        fn jni_va_test_four_named();
    }

    /// Reads back: two integers, a double, and eight more integers — enough to
    /// run past System V's six-GP register budget into the overflow area.
    #[cfg(target_arch = "x86_64")]
    extern "C" fn jni_va_test_sink3(a: u64, b: u64, c: u64, va: VaList) -> u64 {
        let mut cur = VaCursor::new(va);
        unsafe {
            let i1 = cur.next(false);
            let d = f64::from_bits(cur.next(true));
            let mut tail = 0u64;
            for _ in 0..8 {
                tail = tail.wrapping_add(cur.next(false));
            }
            assert_eq!((a, b, c), (11, 22, 33), "named args");
            assert_eq!(i1, 40, "first variadic integer");
            // Bit equality: this asserts a varargs ABI round trip, so any
            // difference at all is a marshalling defect.
            assert_eq!(d.to_bits(), 1.5f64.to_bits(), "variadic double: {d}");
            assert_eq!(tail, 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8, "spilled integers");
        }
        42
    }

    #[cfg(target_arch = "x86_64")]
    extern "C" fn jni_va_test_sink4(a: u64, b: u64, c: u64, d: u64, va: VaList) -> u64 {
        let mut cur = VaCursor::new(va);
        unsafe {
            let i1 = cur.next(false);
            let f = f64::from_bits(cur.next(true));
            let i2 = cur.next(false);
            assert_eq!((a, b, c, d), (11, 22, 33, 44), "named args");
            assert_eq!((i1, i2), (40, 2), "variadic integers");
            assert_eq!(f.to_bits(), 2.5f64.to_bits(), "variadic double: {f}");
        }
        43
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn jni_varargs_trampoline_delivers_the_c_argument_list() {
        let three: unsafe extern "C" fn(u64, u64, u64, ...) -> u64 =
            unsafe { std::mem::transmute(jni_va_test_three_named as *const ()) };
        let r3 = unsafe {
            three(
                11, 22, 33, 40u64, 1.5f64, 1u64, 2u64, 3u64, 4u64, 5u64, 6u64, 7u64, 8u64,
            )
        };
        assert_eq!(r3, 42, "the sink's return must pass back through RAX");

        let four: unsafe extern "C" fn(u64, u64, u64, u64, ...) -> u64 =
            unsafe { std::mem::transmute(jni_va_test_four_named as *const ()) };
        let r4 = unsafe { four(11, 22, 33, 44, 40u64, 2.5f64, 2u64) };
        assert_eq!(r4, 43);
    }

    /// The System V `va_list` is a register-save-area walk with two
    /// INDEPENDENT cursors, not an array of arguments. Reading it as an array
    /// returns its own `gp_offset`/`fp_offset` header as argument one.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    #[test]
    fn sysv_va_cursor_walks_both_register_classes_then_the_overflow_area() {
        // 6 GP slots of 8 bytes, then 8 SSE slots of 16.
        let mut save = [0u64; 6 + 16];
        for (i, w) in save.iter_mut().take(6).enumerate() {
            *w = 100 + i as u64;
        }
        save[6] = 1.5f64.to_bits(); // xmm0
        save[8] = 2.5f64.to_bits(); // xmm1
        let mut overflow = [900u64, 901];
        let mut tag = SysVVaListTag {
            gp_offset: 8, // one named integer argument already consumed
            fp_offset: 48,
            overflow_arg_area: overflow.as_mut_ptr() as *mut u8,
            reg_save_area: save.as_mut_ptr() as *mut u8,
        };
        let mut cur = VaCursor::new(&mut tag as *mut SysVVaListTag as VaList);
        unsafe {
            assert_eq!(cur.next(false), 101, "second GP register, not the first");
            assert_eq!(f64::from_bits(cur.next(true)), 1.5, "xmm0");
            assert_eq!(cur.next(false), 102, "GP advances independently of SSE");
            assert_eq!(f64::from_bits(cur.next(true)), 2.5, "xmm1");
            for want in [103, 104, 105] {
                assert_eq!(cur.next(false), want);
            }
            // GP register budget exhausted: the rest come off the stack.
            assert_eq!(cur.next(false), 900);
            assert_eq!(cur.next(false), 901);
        }
    }

    /// gc-common w2-c (`common-a-jni-placeholder-thread-id-at-barrier`): JNI
    /// `MonitorEnter`/`MonitorExit` own the monitor under the CALLER's registry
    /// id. They used the placeholder `ThreadId(0)` -- main's real id -- so every
    /// native thread shared main's ownership (two threads could both hold one
    /// monitor) and a native re-entering a monitor its own Java caller held
    /// contended with itself. Without the fix `holds(obj, worker)` is false.
    #[test]
    fn jni_monitor_enter_owns_the_monitor_under_the_callers_real_id() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::atomic::Ordering;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "jni-worker", None);
        assert_ne!(worker, ThreadId(0), "the worker must not be main's id");
        let mut thread = JvmThread::new(worker, "jni-worker");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);

        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let jobj = obj_to_jobject(obj);
        assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
        assert!(
            shared.threads.monitors.holds(obj, worker),
            "JNI MonitorEnter must own the monitor under the caller's id"
        );
        assert!(
            !shared.threads.monitors.holds(obj, ThreadId(0)),
            "and not under main's"
        );
        // Re-entrant under the same identity, balanced by two exits.
        assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
        assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
        assert!(shared.threads.monitors.holds(obj, worker));
        assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
        assert!(!shared.threads.monitors.holds(obj, worker));
        // An uncontended acquire by a counted mutator never raises its flag.
        assert!(!thread
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire));
    }

    /// gc-common w2-c (`common-g-pinning-semantics-differ-per-backend`): the
    /// `GetPrimitiveArrayCritical` copy-back follows the array through a
    /// relocation, on Generational too, via the remappable global ref minted at
    /// Get -- and the global ref is dropped again at the final release.
    ///
    /// The relocation is simulated the way the collector applies one to the
    /// global-ref table (`update_after_gc` with a pointer map). Before w2-c the
    /// release re-resolved the raw Get-time handle, so the element landed in the
    /// OLD array (from-space / recycled memory after a real move).
    #[test]
    fn critical_copy_back_follows_the_array_through_a_relocation() {
        use crate::config::{GcAlgorithm, VmConfig};
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        }));
        let _jni = JniTls::context(&shared);
        let old = shared.mem.heap.alloc_array(
            crate::classloading::ClassId::new(0),
            ArrayElementType::Int,
            4,
        );
        let moved = shared.mem.heap.alloc_array(
            crate::classloading::ClassId::new(0),
            ArrayElementType::Int,
            4,
        );
        let refs_before = shared.natives.jni_global_refs.lock().count();

        let mut is_copy: JBoolean = 0;
        let buf = jni_get_primitive_array_critical(get_jni_env(), obj_to_jobject(old), &mut is_copy)
            as *mut i32;
        assert!(!buf.is_null());
        assert_eq!(is_copy, JNI_TRUE, "the critical handout is always a copy");
        assert_eq!(
            shared.natives.jni_global_refs.lock().count(),
            refs_before + 1,
            "the section holds one global ref: the array's keep-alive root"
        );
        unsafe { *buf.add(2) = 77 };

        // The collector relocates `old` to `moved` inside the section.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old.as_ptr() as usize, moved.as_ptr() as usize);
        shared.natives.jni_global_refs.lock().update_after_gc(&map);

        jni_release_primitive_array_critical(
            get_jni_env(),
            obj_to_jobject(old),
            buf as *mut std::ffi::c_void,
            0,
        );
        assert!(
            matches!(
                shared.mem.heap.get_array_element(moved, 2),
                Ok(Value::Int(77))
            ),
            "the copy-back must land where the array IS at Release"
        );
        assert!(
            matches!(shared.mem.heap.get_array_element(old, 2), Ok(Value::Int(0))),
            "and not at the address it had at Get"
        );
        assert_eq!(
            shared.natives.jni_global_refs.lock().count(),
            refs_before,
            "the final release drops the global ref"
        );
    }

    /// gc-common w8-c: two open `GetPrimitiveArrayCritical` sections on EMPTY
    /// arrays get distinct buffers, and releasing both (in either order)
    /// drops both keep-alive global refs. Before, both handouts were the one
    /// dangling sentinel of an empty `Vec`, the second registration replaced
    /// the first, and the first array's global ref -- a strong root -- was
    /// never deleted.
    #[test]
    fn nested_criticals_on_empty_arrays_release_both_roots() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let _jni = JniTls::context(&shared);
        let alloc = || {
            shared.mem.heap.alloc_array(
                crate::classloading::ClassId::new(0),
                ArrayElementType::Int,
                0,
            )
        };
        let (src, dst) = (alloc(), alloc());
        let refs_before = shared.natives.jni_global_refs.lock().count();
        let env = get_jni_env();
        let a = jni_get_primitive_array_critical(env, obj_to_jobject(src), std::ptr::null_mut());
        let b = jni_get_primitive_array_critical(env, obj_to_jobject(dst), std::ptr::null_mut());
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b, "each open critical has its own buffer");
        assert_eq!(shared.natives.jni_global_refs.lock().count(), refs_before + 2);
        jni_release_primitive_array_critical(env, obj_to_jobject(dst), b, 0);
        jni_release_primitive_array_critical(env, obj_to_jobject(src), a, 2);
        assert_eq!(
            shared.natives.jni_global_refs.lock().count(),
            refs_before,
            "both sections' keep-alive roots are released"
        );
        assert_eq!(critical_buffer_bytes(0, 4), 1);
        assert_eq!(critical_buffer_bytes(3, 4), 12);
    }

    /// A caller the census already EXCLUDES (an idle foreign-attached thread, a
    /// `host_thread_enter_native` thread) acquires without touching the barrier
    /// and stays excluded. Running the blocking protocol there would clear its
    /// flag on the way out and turn a thread parked in host code back into a
    /// counted mutator that no later pause can wait out.
    #[test]
    fn jni_monitor_enter_on_an_excluded_caller_leaves_it_excluded() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::atomic::Ordering;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "idle-attached", None);
        let mut thread = JvmThread::new(worker, "idle-attached");
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let blocked_before = shared.mem.gc_barrier.blocked_count();

        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let jobj = obj_to_jobject(obj);
        assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
        assert!(shared.threads.monitors.holds(obj, worker));
        assert!(
            thread
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire),
            "an excluded caller must stay excluded"
        );
        assert_eq!(shared.mem.gc_barrier.blocked_count(), blocked_before);
        assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
        assert!(!shared.threads.monitors.holds(obj, worker));
    }

    /// gc-common w4-g (`common-w3a-jni-unbound-counted-monitor-enter`, residual
    /// 1): an UNBOUND JNI caller that is a registered thread on this OS thread
    /// resolves its own published `JvmThread`, so `MonitorEnter` takes the
    /// GC-safe acquire (deposit + `in_blocked_region`) instead of sleeping in
    /// `block_enter` as a counted thread no pause can wait out. An unpublished
    /// entry, and another OS thread, resolve nothing.
    #[test]
    fn an_unbound_registered_caller_resolves_its_own_jvm_thread() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "unbound-native", None);
        shared.threads.thread_registry.set_os_tid_current(worker);
        let mut thread = JvmThread::new(worker, "unbound-native");
        assert_eq!(
            unbound_caller_jvm_thread(&shared),
            None,
            "no published JvmThread: nothing to adopt"
        );
        shared
            .threads
            .thread_registry
            .set_jvm_thread_addr(worker, &thread as *const JvmThread as usize);
        assert_eq!(
            unbound_caller_jvm_thread(&shared),
            Some(&mut thread as *mut JvmThread),
            "the caller's own published JvmThread"
        );
        let other = {
            let shared = shared.clone();
            std::thread::spawn(move || unbound_caller_jvm_thread(&shared).is_none())
                .join()
                .expect("probe thread must not panic")
        };
        assert!(other, "another OS thread claims no entry");

        // And `MonitorEnter` from the unbound caller takes the bound path:
        // owned under the caller's id, uncontended, flag left down.
        let _jni = JniTls::context(&shared);
        clear_jni_thread();
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let jobj = obj_to_jobject(obj);
        assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
        assert!(shared.threads.monitors.holds(obj, worker));
        assert!(!thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
        assert!(!shared.threads.monitors.holds(obj, worker));
        shared.threads.thread_registry.clear_tlab_addr(worker);
    }

    /// Round 11 wave 8 (`r11w7-lock-jni-unresolved-caller-locks-as-main`): a
    /// JNI caller the registry cannot name (no binding, no entry on its OS
    /// thread) locked AND released as `ThreadId(0)`, which is `main`'s real id.
    /// Its enter is now owned under an unregistered id of its own, stable per
    /// OS thread and distinct across threads, and its exit can no longer
    /// release a lock `main` holds.
    #[test]
    fn an_unnamed_jni_caller_does_not_lock_or_unlock_as_main() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let jobj = obj_to_jobject(obj);

        // One unnamed thread: enter, observe, exit. Answers its owner id.
        let unnamed_round_trip = move |shared: Arc<SharedVm>| {
            std::thread::spawn(move || {
                set_jni_context_arc(shared.clone());
                clear_jni_thread();
                let obj = jobject_to_obj(jobj).expect("a live local ref");
                assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
                let owner = unnamed_caller_monitor_owner(&shared, false)
                    .expect("MonitorEnter issued this thread an owner id");
                assert_ne!(owner, ThreadId(0), "never main's id");
                assert!(!shared.threads.thread_registry.is_alive(owner));
                assert!(shared.threads.monitors.holds(obj, owner));
                assert!(
                    !shared.threads.monitors.holds(obj, ThreadId(0)),
                    "the unnamed caller's lock must not read as main's"
                );
                // Re-entrant under the same id, balanced by two exits.
                assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
                assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
                assert!(shared.threads.monitors.holds(obj, owner));
                assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
                assert!(!shared.threads.monitors.holds(obj, owner));
                clear_jni_context();
                owner
            })
            .join()
            .expect("unnamed JNI caller must not panic")
        };
        let first = unnamed_round_trip(shared.clone());
        let second = unnamed_round_trip(shared.clone());
        assert_ne!(
            first, second,
            "each unnamed OS thread owns under its own id"
        );

        // `main` holds the object; an unnamed thread's stray `MonitorExit`
        // must leave it held (it used to release under `ThreadId(0)`).
        shared.threads.monitors.enter(obj, ThreadId(0));
        let s = shared.clone();
        std::thread::spawn(move || {
            set_jni_context_arc(s);
            clear_jni_thread();
            assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
            clear_jni_context();
        })
        .join()
        .expect("unnamed JNI caller must not panic");
        assert!(
            shared.threads.monitors.holds(obj, ThreadId(0)),
            "an unnamed caller's MonitorExit released main's lock"
        );
        assert!(
            shared.threads.monitors.exit(obj, ThreadId(0)).is_ok(),
            "main still owns it"
        );
    }

    /// Round 11 wave 8 (`r11w4-sync-jni-excluded-monitor-ops-touch-a-movable-header`):
    /// a census-EXCLUDED caller's `MonitorEnter` does not touch the mark word
    /// while a pause is in progress (no pause waits for it, so a moving
    /// collection could relocate the object under its CAS); it acquires once
    /// the pause completes, and stays excluded.
    #[test]
    fn an_excluded_jni_monitor_enter_waits_out_an_active_pause() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::atomic::{AtomicBool, Ordering};
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        let worker = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(worker, "idle-attached", None);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let jobj = obj_to_jobject(obj);

        // A pause initiated by `main` that excludes the worker.
        assert!(shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(main_tid, || (2, 1, vec![worker.0])));

        let entered = Arc::new(AtomicBool::new(false));
        let (s, e) = (shared.clone(), entered.clone());
        let h = std::thread::spawn(move || {
            let mut thread = JvmThread::new(worker, "idle-attached");
            thread
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
            set_jni_context_arc(s.clone());
            set_jni_thread(&mut thread as *mut JvmThread);
            assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
            e.store(true, Ordering::Release);
            let still_excluded = thread
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire);
            assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
            clear_jni_thread();
            clear_jni_context();
            still_excluded
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::Acquire),
            "an excluded MonitorEnter must not complete during a pause"
        );
        assert!(
            !shared.threads.monitors.holds(obj, worker),
            "nor touch the mark word"
        );
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        assert!(h.join().expect("excluded JNI caller must not panic"));
        assert!(entered.load(Ordering::Acquire));
        assert!(!shared.threads.monitors.holds(obj, worker));
    }

    /// gc-common w6-g (`common-w3a-jni-unbound-counted-monitor-enter`,
    /// residual 2): on a carrier, the OS tid is claimed by the parked virtual
    /// thread that ran there before as well as by the mounted one. An unbound
    /// caller resolves to the MOUNTED one (the newest publish on this OS
    /// thread), never to the parked one, and `MonitorEnter` is owned under
    /// that identity.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn an_unbound_mounted_virtual_thread_resolves_past_the_parked_ones() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let registry = &shared.threads.thread_registry;
        let parked = registry.next_thread_id();
        registry.register(parked, "vt-parked", None);
        let mounted = registry.next_thread_id();
        registry.register(mounted, "vt-mounted", None);
        // Both mounted on THIS OS thread, `parked` first; nothing clears its
        // claim at unmount.
        let parked_thread = Box::new(JvmThread::new(parked, "vt-parked"));
        registry.set_os_tid_current(parked);
        registry.set_jvm_thread_addr(parked, &*parked_thread as *const JvmThread as usize);
        let mut thread = JvmThread::new(mounted, "vt-mounted");
        registry.set_os_tid_current(mounted);
        registry.set_jvm_thread_addr(mounted, &thread as *const JvmThread as usize);
        assert_eq!(registry.thread_id_for_current_os_tid(), Some(mounted));
        assert_eq!(
            unbound_caller_jvm_thread(&shared),
            Some(&mut thread as *mut JvmThread),
            "the mounted thread's own JvmThread, not the parked one's",
        );

        let _jni = JniTls::context(&shared);
        clear_jni_thread();
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let jobj = obj_to_jobject(obj);
        assert_eq!(jni_monitor_enter(get_jni_env(), jobj), JNI_OK);
        assert!(shared.threads.monitors.holds(obj, mounted));
        assert!(!shared.threads.monitors.holds(obj, parked));
        assert_eq!(jni_monitor_exit(get_jni_env(), jobj), JNI_OK);
        assert!(!shared.threads.monitors.holds(obj, mounted));
        registry.clear_tlab_addr(mounted);
        registry.clear_tlab_addr(parked);
        drop(parked_thread);
    }

    /// gc-common w4-g (`handoff-w2d-finalizer-registration-native`, JNI half):
    /// the `NewObject*` registration point registers a finalizable instance
    /// exactly once and a non-finalizable one never; `AllocObject` -- no
    /// constructor runs -- never registers (HotSpot `RegisterFinalizersAtInit`).
    #[test]
    fn new_object_registers_a_finalizable_instance_and_alloc_object_does_not() {
        use crate::config::VmConfig;
        use crate::vm::Vm;
        let mut vm = Vm::new(VmConfig::default());
        let class_id = vm
            .shared
            .load_class_concurrent("java/lang/StringBuilder")
            .expect("load StringBuilder");
        let num_fields = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .expect("loaded");
        let rows = |shared: &SharedVm, obj: ObjectRef| {
            shared
                .mem
                .ref_processor
                .lock()
                .finalizer_referent_addresses()
                .into_iter()
                .filter(|&a| a == obj.as_ptr() as usize)
                .count()
        };
        let plain = vm.shared.mem.heap.alloc_object(class_id, num_fields);
        assert!(!register_if_constructed_finalizable(&vm.shared, plain, class_id));
        assert_eq!(rows(&*vm.shared, plain), 0, "not finalizable: no row");

        // This VM's class store only: pretend the class overrides finalize().
        vm.shared
            .classes
            .class_manager
            .write()
            .class_store
            .get_mut(class_id)
            .expect("loaded")
            .has_finalizer = true;

        let _jni = JniTls::replace(&vm.shared).with_thread(vm.main_thread.as_mut() as *mut _);
        let h = jni_alloc_object(get_jni_env(), class_id_to_jclass(class_id));
        let allocated = jobject_to_obj(h).expect("AllocObject allocates");
        assert_eq!(rows(&*vm.shared, allocated), 0, "AllocObject must not register");

        let constructed = vm.shared.mem.heap.alloc_object(class_id, num_fields);
        assert!(register_if_constructed_finalizable(&vm.shared, constructed, class_id));
        assert_eq!(rows(&*vm.shared, constructed), 1, "registered exactly once");
    }

    /// gc-common w4-g: `ExceptionOccurred` hands out a RECORDED local ref, so
    /// the throwable stays rooted after `ExceptionClear` drops
    /// `native_pending_return` (the `t = ExceptionOccurred(); ExceptionClear();
    /// ... use t` idiom).
    #[test]
    fn exception_occurred_hands_out_a_rooted_local_ref() {
        use crate::config::VmConfig;
        use crate::vm::Vm;

        let mut vm = Vm::new(VmConfig::default());
        let class_id = vm
            .shared
            .load_class_concurrent("java/lang/IllegalArgumentException")
            .expect("load IllegalArgumentException");
        let message = CString::new("w4-g").unwrap();
        let _jni = JniTls::replace(&vm.shared).with_thread(vm.main_thread.as_mut() as *mut _);
        push_local_frame(4); // the native's implicit frame
        assert_eq!(
            jni_throw_new(get_jni_env(), class_id_to_jclass(class_id), message.as_ptr()),
            JNI_OK
        );
        let t = jni_exception_occurred(get_jni_env());
        assert_ne!(t, 0, "ThrowNew must leave a pending exception");
        jni_exception_clear(get_jni_env());
        assert!(vm.main_thread.native_pending_return.is_none());
        let mut roots = Vec::new();
        collect_local_ref_roots(&mut roots);
        assert!(
            roots.iter().any(|r| obj_to_jobject(*r) == t),
            "after ExceptionClear the native's throwable must still be rooted"
        );
        let _ = pop_local_frame(0);
    }

    // -----------------------------------------------------------------------
    // gc-common w10-c
    // -----------------------------------------------------------------------

    /// A `SharedVm` of `gc_algorithm` with its `self_arc` installed, as
    /// `Vm::new` installs it, so a foreign attachment records it
    /// (`FOREIGN_ATTACH_VM`), plus a registered "main" initiator.
    fn w10c_vm(gc_algorithm: crate::config::GcAlgorithm) -> (Arc<SharedVm>, ThreadId) {
        use crate::config::VmConfig;
        let shared = Arc::new(SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        }));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let main_tid = shared.threads.thread_registry.next_thread_id();
        shared
            .threads
            .thread_registry
            .register(main_tid, "main", None);
        (shared, main_tid)
    }

    /// Ends a pause the test opened, on a failing assertion's unwind too, so
    /// the foreign thread (which waits the pause out) can finish.
    struct PauseHeld<'a>(&'a SharedVm, bool);

    impl PauseHeld<'_> {
        fn complete(&mut self) {
            if !self.1 {
                self.1 = true;
                self.0
                    .mem
                    .gc_barrier
                    .complete_gc(cratonvm_types::PointerMap::default());
            }
        }
    }

    impl Drop for PauseHeld<'_> {
        fn drop(&mut self) {
            self.complete();
        }
    }

    /// `DetachCurrentThread` for a test attachment, minus the `ThreadLocal`
    /// release (`release_foreign_thread_locals` builds the `Thread` mirror,
    /// which needs JDK classes the unit-test VM does not have): the same
    /// barrier-serialized retire + mark-dead, then `detach_foreign_thread`,
    /// then the JNI TLS.
    fn w10c_detach(shared: &SharedVm) {
        if let Some(tid) = with_foreign_thread(|jt| jt.thread_id) {
            shared.mem.gc_barrier.mark_blocked_region_leave_after(|| {
                with_foreign_thread(|jt| jt.tlab.retire());
                shared.threads.thread_registry.clear_tlab_addr(tid);
                shared.threads.thread_registry.mark_dead(tid);
            });
            detach_foreign_thread(shared);
        }
        clear_jni_thread();
        clear_jni_context();
    }

    /// Is this OS thread's foreign attachment in the idle (GC-blocked) state?
    fn foreign_is_idle() -> bool {
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire)
        })
        .unwrap_or(false)
            && FOREIGN_CALL_DEPTH.with(Cell::get) == 0
    }

    /// `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked`, with
    /// `CRATONVM_JNI_FOREIGN_TRANSITIONS` (and indirect locals) on: an
    /// attached-but-idle host thread builds a callback's arguments the JNA way
    /// (`NewStringUTF`, `NewByteArray`, `SetByteArrayRegion`) with no Java call
    /// in between, another thread forces a REAL full collection of the VM while
    /// the host thread sits idle, and the host thread's unchanged handles then
    /// read back intact. Before the fix those objects were rooted by nothing
    /// (no open frame, and a snapshot deposited before they existed).
    fn idle_foreign_jni_objects_survive_a_collection_on(gc_algorithm: crate::config::GcAlgorithm) {
        use std::sync::mpsc;
        use std::time::Duration;
        const WAIT: Duration = Duration::from_secs(30);
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, main_tid) = w10c_vm(gc_algorithm);
        let mut main = JvmThread::new(main_tid, "w10c-main");
        let (to_main, from_foreign) = mpsc::channel::<Result<(), String>>();
        let (to_foreign, from_main) = mpsc::channel::<()>();
        let vm = Arc::clone(&shared);
        let foreign = std::thread::spawn(move || {
            FOREIGN_TRANSITIONS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            INDIRECT_LOCALS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("w10c-foreign"));
            if local_frame_depth() != 1 {
                w10c_detach(&vm);
                return Err("the attach-level frame is open".to_string());
            }
            let env = get_jni_env();
            let check = || -> Result<(JObject, JObject), String> {
                if !foreign_is_idle() {
                    return Err("an attached thread starts idle".into());
                }
                let text = CString::new("w10-c idle foreign").unwrap();
                let s = jni_new_string_utf(env, text.as_ptr());
                let b = jni_new_byte_array(env, 3);
                jni_set_byte_array_region(env, b, 0, 3, [1 as JByte, 2, 3].as_ptr());
                if s == 0 || b == 0 {
                    return Err("NewStringUTF / NewByteArray answered NULL".into());
                }
                if !is_indirect_local_tag(s) || !is_indirect_local_tag(b) {
                    return Err("attach-level locals are indirect with both flags on".into());
                }
                if !foreign_is_idle() {
                    return Err("each JNI function returns the thread to idle".into());
                }
                Ok((s, b))
            };
            let handles = check();
            let _ = to_main.send(handles.as_ref().map(|_| ()).map_err(String::clone));
            if handles.is_ok() && from_main.recv_timeout(WAIT).is_err() {
                w10c_detach(&vm);
                return Err("the collection never ran".to_string());
            }
            let result = handles.and_then(|(s, b)| {
                // Through the same handles, after the collection(s).
                let text = jstring_text(env, s);
                if text.as_deref() != Some("w10-c idle foreign") {
                    return Err(format!("{gc_algorithm:?}: string read back as {text:?}"));
                }
                let mut out = [0 as JByte; 3];
                jni_get_byte_array_region(env, b, 0, 3, out.as_mut_ptr());
                if out != [1, 2, 3] || jni_get_array_length(env, b) != 3 {
                    return Err(format!("{gc_algorithm:?}: byte[] read back as {out:?}"));
                }
                Ok(())
            });
            w10c_detach(&vm);
            if is_foreign_attached() {
                return Err("detach failed".to_string());
            }
            if local_frame_depth() != 0 {
                return Err("detach closes the attach-level frame".to_string());
            }
            result
        });
        let allocated = from_foreign
            .recv_timeout(WAIT)
            .expect("the foreign thread reports");
        allocated.expect("the idle foreign thread's JNI calls");
        // Two full collections while the host thread is idle (excluded from
        // both pauses): its roots are its deposited snapshot, the attach-level
        // frame included; its handles are remapped by its next leave.
        let cycles = shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed);
        crate::runtime::interpreter::force_gc_for_vm(&shared, &mut main);
        crate::runtime::interpreter::force_gc_for_vm(&shared, &mut main);
        assert!(
            shared
                .mem
                .gc_cycle_count
                .load(std::sync::atomic::Ordering::Relaxed)
                > cycles,
            "{gc_algorithm:?}: a collection ran"
        );
        to_foreign.send(()).expect("the foreign thread waits");
        foreign
            .join()
            .expect("the foreign thread does not panic")
            .expect("the idle foreign thread's objects survive the collection");
    }

    #[test]
    fn idle_foreign_jni_objects_survive_a_generational_collection() {
        idle_foreign_jni_objects_survive_a_collection_on(crate::config::GcAlgorithm::Generational);
    }

    #[test]
    fn idle_foreign_jni_objects_survive_a_g1_collection() {
        idle_foreign_jni_objects_survive_a_collection_on(crate::config::GcAlgorithm::G1);
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn idle_foreign_jni_objects_survive_a_zgc_collection() {
        idle_foreign_jni_objects_survive_a_collection_on(crate::config::GcAlgorithm::Zgc);
    }

    /// The other half of the same page: with the transitions on, a JNI
    /// function an idle foreign thread calls while another thread holds a
    /// stop-the-world pause does not run until the pause ends. Before the fix
    /// it allocated in the middle of the pause (the thread was excluded from
    /// it and nothing made it wait).
    #[test]
    fn an_idle_foreign_jni_function_waits_out_a_pause() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;
        const WAIT: Duration = Duration::from_secs(30);
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let (to_main, from_foreign) = mpsc::channel::<&'static str>();
        let (to_foreign, from_main) = mpsc::channel::<()>();
        let returned = Arc::new(AtomicBool::new(false));
        let vm = Arc::clone(&shared);
        let done = Arc::clone(&returned);
        let foreign = std::thread::spawn(move || {
            FOREIGN_TRANSITIONS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("w10c-pause"));
            let _ = to_main.send("attached");
            if from_main.recv_timeout(WAIT).is_ok() {
                let text = CString::new("after the pause").unwrap();
                let s = jni_new_string_utf(get_jni_env(), text.as_ptr());
                done.store(true, Ordering::SeqCst);
                let _ = to_main.send(if s != 0 { "allocated" } else { "null" });
                let _ = from_main.recv_timeout(WAIT);
            }
            w10c_detach(&vm);
        });
        assert_eq!(from_foreign.recv_timeout(WAIT), Ok("attached"));
        let alive = shared.threads.thread_registry.alive_count() as u32;
        assert_eq!(alive, 2, "main and the attached thread");
        assert!(shared.mem.gc_barrier.request_stw(main_tid, alive));
        let mut pause = PauseHeld(&shared, false);
        assert_eq!(
            shared.mem.gc_barrier.pending_count(),
            0,
            "the idle foreign thread is excluded from the pause"
        );
        shared.mem.gc_barrier.wait_for_all();
        to_foreign.send(()).expect("the foreign thread waits");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !returned.load(Ordering::SeqCst),
            "NewStringUTF must not run while another thread's pause holds"
        );
        pause.complete();
        assert_eq!(from_foreign.recv_timeout(WAIT), Ok("allocated"));
        assert!(returned.load(Ordering::SeqCst));
        let _ = to_foreign.send(());
        foreign.join().expect("the foreign thread does not panic");
        drop(pause);
    }

    // -----------------------------------------------------------------------
    // gcd d3/k: a Java thread inside a JNI native method
    // (`CRATONVM_JNI_NATIVE_TRANSITIONS`)
    // -----------------------------------------------------------------------

    /// Restores this thread's transition overrides on drop (an assertion's
    /// unwind included), so no test leaves one set for a later test that the
    /// harness runs on the same OS thread.
    struct TransitionOverrides;

    impl TransitionOverrides {
        fn set(native: Option<bool>, indirect: Option<bool>, foreign: Option<bool>) -> Self {
            JNI_NATIVE_CALL.with(|t| t.test_override.set(native));
            INDIRECT_LOCALS_TEST_OVERRIDE.with(|c| c.set(indirect));
            FOREIGN_TRANSITIONS_TEST_OVERRIDE.with(|c| c.set(foreign));
            TransitionOverrides
        }
    }

    impl Drop for TransitionOverrides {
        fn drop(&mut self) {
            let _ = JNI_NATIVE_CALL.try_with(|t| t.test_override.set(None));
            let _ = INDIRECT_LOCALS_TEST_OVERRIDE.try_with(|c| c.set(None));
            let _ = FOREIGN_TRANSITIONS_TEST_OVERRIDE.try_with(|c| c.set(None));
        }
    }

    /// A registered Java thread of `shared` for a dispatch test: its registry
    /// entry shares the thread's `gc_block_state` and `root_snapshot`, as a
    /// started thread's does. The caller marks it dead when done.
    fn d3k_java_thread(shared: &SharedVm, name: &str) -> Box<JvmThread> {
        let registry = &shared.threads.thread_registry;
        let tid = registry.next_thread_id();
        let jt = Box::new(JvmThread::new(tid, name));
        registry.register(tid, name, None);
        registry.set_gc_block_state(tid, jt.gc_block_state.clone());
        registry.set_root_snapshot(tid, jt.root_snapshot.clone());
        jt
    }

    fn d3k_in_native(jt: *const JvmThread) -> bool {
        // SAFETY: the test's own boxed `JvmThread`; only an atomic is read.
        unsafe { &*jt }
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `gcd-d2i-jni-native-methods-are-counted-mutators`: with the native
    /// transitions and indirect locals on, a Java thread inside a JNI native
    /// (a `JniNativeCall`, as `dispatch_jni_native` opens it around the C
    /// call) is excluded from another thread's stop-the-world pause -- the
    /// pause completes its census without it -- and reads RUNNABLE; a JNIEnv
    /// function it calls while the pause holds does not run until the pause
    /// ends, hands out an indirect local, and returns the thread to native;
    /// the call's end takes it out of native. Before the fix the thread was a
    /// counted mutator for the whole call: the pause waited for the native.
    #[test]
    fn a_java_thread_in_a_jni_native_holds_no_pause_and_its_jni_functions_wait_one_out() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;
        const WAIT: Duration = Duration::from_secs(30);
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let (to_main, from_native) = mpsc::channel::<Result<&'static str, String>>();
        let (to_native, from_main) = mpsc::channel::<()>();
        let returned = Arc::new(AtomicBool::new(false));
        let vm = Arc::clone(&shared);
        let done = Arc::clone(&returned);
        let native = std::thread::spawn(move || -> Result<(), String> {
            let _flags = TransitionOverrides::set(Some(true), Some(true), None);
            let mut jt = d3k_java_thread(&vm, "d3k-native");
            let tid = jt.thread_id;
            let jt_ptr: *mut JvmThread = &mut *jt;
            let result = {
                let _jni = JniTls::context(&vm).with_thread(jt_ptr);
                // The dispatch's implicit local frame.
                push_local_frame(16);
                let run = || -> Result<(), String> {
                    let call = JniNativeCall::enter(raw_local_escapes());
                    if !d3k_in_native(jt_ptr) {
                        return Err("the native call went into native".into());
                    }
                    if vm.threads.thread_registry.java_block_state(tid) != 0 {
                        return Err("a thread in native reads RUNNABLE".into());
                    }
                    let _ = to_main.send(Ok("in native"));
                    if from_main.recv_timeout(WAIT).is_err() {
                        return Err("the pause never came".into());
                    }
                    let text = CString::new("d3k in native").unwrap();
                    let s = jni_new_string_utf(get_jni_env(), text.as_ptr());
                    done.store(true, Ordering::SeqCst);
                    if s == 0 || !is_indirect_local_tag(s) {
                        return Err(format!(
                            "NewStringUTF answered {s:#x}, not an indirect local"
                        ));
                    }
                    if !d3k_in_native(jt_ptr) {
                        return Err("a JNIEnv function returns the thread to native".into());
                    }
                    if jstring_text(get_jni_env(), s).as_deref() != Some("d3k in native") {
                        return Err("the local reads back".into());
                    }
                    drop(call);
                    if d3k_in_native(jt_ptr) {
                        return Err("the call's end leaves native".into());
                    }
                    Ok(())
                };
                let r = run();
                if let Err(e) = &r {
                    let _ = to_main.send(Err(e.clone()));
                }
                r
            };
            vm.threads.thread_registry.mark_dead(tid);
            drop(jt);
            result
        });
        match from_native.recv_timeout(WAIT) {
            Ok(Ok("in native")) => {}
            other => {
                let _ = to_native.send(());
                let joined = native.join();
                panic!("the native thread did not reach native: {other:?} / {joined:?}");
            }
        }
        let alive = shared.threads.thread_registry.alive_count() as u32;
        assert_eq!(alive, 2, "main and the thread in native");
        assert!(shared.mem.gc_barrier.request_stw(main_tid, alive));
        let mut pause = PauseHeld(&shared, false);
        assert_eq!(
            shared.mem.gc_barrier.pending_count(),
            0,
            "the thread in native is excluded from the pause"
        );
        shared.mem.gc_barrier.wait_for_all();
        to_native.send(()).expect("the native thread waits");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !returned.load(Ordering::SeqCst),
            "NewStringUTF must not run while another thread's pause holds"
        );
        pause.complete();
        native
            .join()
            .expect("the native thread does not panic")
            .expect("the in-native protocol");
        assert!(returned.load(Ordering::SeqCst));
        drop(pause);
    }

    /// The call stays a counted mutator -- byte-for-byte the old path --
    /// whenever a moving collection in the native window could strand a raw
    /// local the native holds: the flag off, indirect locals off (the default
    /// representation), or a receiver/argument recorded raw. And a call a
    /// JNIEnv function handed a raw local stays counted from that function on.
    #[test]
    fn a_jni_native_call_stays_counted_wherever_a_raw_local_could_escape() {
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut jt = d3k_java_thread(&shared, "d3k-counted");
        let tid = jt.thread_id;
        let jt_ptr: *mut JvmThread = &mut *jt;
        {
            let _jni = JniTls::context(&shared).with_thread(jt_ptr);
            push_local_frame(16);

            // The flag off: inert.
            let _flags = TransitionOverrides::set(Some(false), Some(true), None);
            let call = JniNativeCall::enter(raw_local_escapes());
            assert!(!d3k_in_native(jt_ptr), "flag off: counted");
            drop(call);
            drop(_flags);

            // Raw locals (indirect off): inert.
            let _flags = TransitionOverrides::set(Some(true), Some(false), None);
            let call = JniNativeCall::enter(raw_local_escapes());
            assert!(!d3k_in_native(jt_ptr), "raw locals: counted");
            drop(call);
            drop(_flags);

            let _flags = TransitionOverrides::set(Some(true), Some(true), None);
            // A receiver or argument that went out raw.
            let before = raw_local_escapes();
            note_raw_local_escape();
            let call = JniNativeCall::enter(before);
            assert!(!d3k_in_native(jt_ptr), "a raw argument: counted");
            drop(call);

            // In native, then a JNIEnv function hands out a raw local.
            let call = JniNativeCall::enter(raw_local_escapes());
            assert!(d3k_in_native(jt_ptr), "both on: in native");
            {
                let fx = ForeignJniEntry::enter();
                assert!(!d3k_in_native(jt_ptr), "a JNIEnv function runs counted");
                note_raw_local_escape();
                drop(fx);
            }
            assert!(
                !d3k_in_native(jt_ptr),
                "a raw escape keeps the call counted"
            );
            {
                // Later functions of the same call are inert.
                let fx = ForeignJniEntry::enter();
                assert!(fx.native.is_none());
            }
            drop(call);
            assert!(!d3k_in_native(jt_ptr));
            assert!(
                JNI_NATIVE_CALL.with(|t| t.record.borrow().is_none()),
                "the call's record is gone"
            );
        }
        shared.threads.thread_registry.mark_dead(tid);
    }

    /// Nesting: a native that Java calls from inside a JNIEnv up-call of an
    /// in-native native brackets its own C call and restores the enclosing
    /// call's record, which the up-call's function then puts back in native.
    #[test]
    fn a_nested_jni_native_call_restores_the_enclosing_record() {
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut jt = d3k_java_thread(&shared, "d3k-nested");
        let tid = jt.thread_id;
        let jt_ptr: *mut JvmThread = &mut *jt;
        {
            let _jni = JniTls::context(&shared).with_thread(jt_ptr);
            let _flags = TransitionOverrides::set(Some(true), Some(true), None);
            push_local_frame(16);
            let outer = JniNativeCall::enter(raw_local_escapes());
            assert!(d3k_in_native(jt_ptr));
            {
                // The outer native's `Call*Method`: out of native...
                let fx = ForeignJniEntry::enter();
                assert!(!d3k_in_native(jt_ptr));
                // ...Java calls another native, which goes into native...
                push_local_frame(16);
                let inner = JniNativeCall::enter(raw_local_escapes());
                assert!(d3k_in_native(jt_ptr), "the inner call is in native");
                drop(inner);
                assert!(!d3k_in_native(jt_ptr), "back in the up-call, counted");
                let outer_back = JNI_NATIVE_CALL
                    .with(|t| t.record.borrow().as_ref().is_some_and(|r| !r.in_native));
                assert!(outer_back, "the outer record is back, out of native");
                drop(fx);
            }
            assert!(d3k_in_native(jt_ptr), "the outer native is in native again");
            drop(outer);
            assert!(!d3k_in_native(jt_ptr));
        }
        shared.threads.thread_registry.mark_dead(tid);
    }

    /// `cratonvm_thread_enter_native` / `_leave_native` (libcratonvm) called
    /// by a native that is already in native are no-ops, like on an attached
    /// thread: the host marking would empty the snapshot the native call
    /// deposited and count the thread blocked twice.
    #[test]
    fn host_native_is_a_no_op_inside_an_in_native_jni_call() {
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut jt = d3k_java_thread(&shared, "d3k-host-native");
        let tid = jt.thread_id;
        let jt_ptr: *mut JvmThread = &mut *jt;
        {
            let _jni = JniTls::context(&shared).with_thread(jt_ptr);
            let _flags = TransitionOverrides::set(Some(true), Some(true), None);
            push_local_frame(16);
            let call = JniNativeCall::enter(raw_local_escapes());
            assert!(d3k_in_native(jt_ptr));
            let blocked = shared.mem.gc_barrier.blocked_count();
            assert!(host_thread_enter_native());
            assert_eq!(
                shared.mem.gc_barrier.blocked_count(),
                blocked,
                "not counted twice"
            );
            assert!(host_thread_leave_native());
            assert_eq!(shared.mem.gc_barrier.blocked_count(), blocked);
            assert!(d3k_in_native(jt_ptr), "still in native");
            drop(call);
            assert!(!d3k_in_native(jt_ptr));
        }
        shared.threads.thread_registry.mark_dead(tid);
    }

    /// gcd d10/j (`gcd-d10j-host-native-inside-a-jni-native-empties-live-roots`):
    /// `cratonvm_thread_enter_native` called from inside a COUNTED JNI native
    /// of a VM thread (the default flags) deposits the thread's real roots --
    /// the native's local here -- instead of emptying its snapshot, counts it
    /// blocked, and the leave wakes it the canonical way. It used to publish
    /// an empty snapshot and exclude the thread from every pause while its
    /// Java frames and locals were live.
    #[test]
    fn host_native_inside_a_counted_jni_native_keeps_the_threads_roots() {
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut jt = d3k_java_thread(&shared, "d10j-host-in-jni");
        let tid = jt.thread_id;
        let jt_ptr: *mut JvmThread = &mut *jt;
        {
            let _jni = JniTls::context(&shared).with_thread(jt_ptr);
            // The defaults: the native call stays counted.
            let _flags = TransitionOverrides::set(Some(false), Some(false), Some(false));
            push_local_frame(16);
            let env = get_jni_env();
            let text = CString::new("d10j host").unwrap();
            let s = jni_new_string_utf(env, text.as_ptr());
            let obj = jobject_to_obj(s).expect("the native's local resolves");
            // What `JniContextGuard::install_for_native` sets for a dispatch.
            // SAFETY: the test's own boxed `JvmThread`; one field written.
            unsafe { (*jt_ptr).jni_native_class = Some(ClassId::new(7)) };
            let blocked = shared.mem.gc_barrier.blocked_count();
            assert!(host_thread_enter_native());
            assert!(d3k_in_native(jt_ptr), "excluded from pauses while in host code");
            assert_eq!(shared.mem.gc_barrier.blocked_count(), blocked + 1);
            assert!(
                // SAFETY: as above; the snapshot's own lock.
                unsafe { &*jt_ptr }.root_snapshot.lock().contains(&obj),
                "the native's local is a root of the host window, not dropped"
            );
            assert!(host_thread_leave_native());
            assert!(!d3k_in_native(jt_ptr), "a counted mutator again");
            assert_eq!(shared.mem.gc_barrier.blocked_count(), blocked);
            // SAFETY: as above.
            unsafe { (*jt_ptr).jni_native_class = None };
        }
        shared.threads.thread_registry.mark_dead(tid);
    }

    /// gcd d5/f: a LEAF JNIEnv function (`GetArrayLength`,
    /// `Set/Get<Int>ArrayRegion`, `DeleteLocalRef`) called by a native in
    /// native runs in a leaf window: the thread never leaves native (its
    /// `in_blocked_region` stays up and the barrier's blocked count never
    /// moves) and the answers are right. Once a relocation was folded into the
    /// thread (simulated through its fold count) the next leaf call takes the
    /// full transition and records a fresh sync point; an out-of-range region
    /// escalates to the full transition before it raises, and the call is back
    /// in native afterwards. The call's end leaves native through the quiet
    /// leave.
    #[test]
    fn a_leaf_jni_function_runs_in_a_window_without_leaving_native() {
        use std::sync::atomic::Ordering;
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut jt = d3k_java_thread(&shared, "d5f-leaf");
        let tid = jt.thread_id;
        let jt_ptr: *mut JvmThread = &mut *jt;
        {
            let _jni = JniTls::context(&shared).with_thread(jt_ptr);
            let _flags = TransitionOverrides::set(Some(true), Some(true), None);
            push_local_frame(16);
            let env = get_jni_env();
            let arr = jni_new_int_array(env, 4);
            assert!(arr != 0 && is_indirect_local_tag(arr), "an indirect local int[4]");
            let call = JniNativeCall::enter(raw_local_escapes());
            assert!(d3k_in_native(jt_ptr));
            let blocked = shared.mem.gc_barrier.blocked_count();
            let leaf_state = || JNI_NATIVE_CALL.with(|t| t.leaf.get());

            assert_eq!(jni_get_array_length(env, arr), 4);
            let src: [JInt; 4] = [7, 8, 9, 10];
            jni_set_int_array_region(env, arr, 0, 4, src.as_ptr());
            let mut dst: [JInt; 4] = [0; 4];
            jni_get_int_array_region(env, arr, 0, 4, dst.as_mut_ptr());
            assert_eq!(dst, src, "the region round-trips");
            assert!(d3k_in_native(jt_ptr), "the leaf calls never left native");
            assert_eq!(
                shared.mem.gc_barrier.blocked_count(),
                blocked,
                "no blocked-region transition"
            );
            assert_eq!(leaf_state(), LEAF_NONE);
            let closed =
                JNI_NATIVE_CALL.with(|t| t.leaf_word.get().is_some_and(|w| !w.is_open()));
            assert!(closed, "the word exists and every window closed");

            // A relocation was folded into the thread: the next leaf call
            // takes the full transition and re-syncs.
            // SAFETY: the test's own boxed `JvmThread`; only an atomic.
            unsafe { &*jt_ptr }
                .gc_block_state
                .blocked_folds
                .fetch_add(1, Ordering::AcqRel);
            assert_eq!(jni_get_array_length(env, arr), 4);
            assert!(d3k_in_native(jt_ptr), "back in native after the full transition");
            let synced = JNI_NATIVE_CALL.with(|t| {
                t.record.borrow().as_ref().is_some_and(|r| {
                    // SAFETY: see `NativeCallRecord::block_state`.
                    r.in_native && r.sync == NativeSync::read(&shared, unsafe { &*r.block_state })
                })
            });
            assert!(synced, "the re-entry recorded a fresh sync point");

            // Out of range: escalates before raising, and is back in native.
            jni_get_int_array_region(env, arr, 2, 5, dst.as_mut_ptr());
            assert!(d3k_in_native(jt_ptr), "back in native after an escalated leaf");
            assert_eq!(leaf_state(), LEAF_NONE);
            assert!(
                take_jni_pending_exception().is_some(),
                "the out-of-range region raised"
            );

            // DeleteLocalRef is a leaf too; the slot then resolves to nothing.
            jni_delete_local_ref(env, arr);
            assert_eq!(jni_get_array_length(env, arr), 0);
            assert!(d3k_in_native(jt_ptr));
            assert_eq!(shared.mem.gc_barrier.blocked_count(), blocked);

            drop(call);
            assert!(!d3k_in_native(jt_ptr), "the call's end leaves native");
            assert_eq!(leaf_state(), LEAF_NONE);
        }
        shared.threads.thread_registry.mark_dead(tid);
    }

    /// gcd d10/j: a JNIEnv function entered through `enter_vm_only`
    /// (`NewStringUTF` here) called by a native in native goes back into
    /// native WITHOUT a second full deposit when its leave was quiet: the
    /// thread is in native again, the snapshot deposited on the way in is
    /// kept and the new local's referent is appended to it (so a collection
    /// in the next window keeps and forwards it), and the record counts the
    /// appends. A function that reached `with_jni_context` (a raise), or a
    /// fold into the thread since the deposit, re-enters with a full deposit
    /// (the count starts again at 0).
    #[test]
    fn a_vm_only_jni_function_reenters_native_without_a_full_deposit() {
        use std::sync::atomic::Ordering;
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut jt = d3k_java_thread(&shared, "d10j-vm-only");
        let tid = jt.thread_id;
        let jt_ptr: *mut JvmThread = &mut *jt;
        let appended = || {
            JNI_NATIVE_CALL.with(|t| t.record.borrow().as_ref().map(|r| r.appended))
        };
        // `move`: the closure copies the raw pointer, so the test can still write
        // through `jt_ptr` below (a by-reference capture of the pointee is a borrow).
        let in_snapshot = move |h: JObject| -> bool {
            let Some(obj) = jobject_to_obj(h) else {
                return false;
            };
            // SAFETY: the test's own boxed `JvmThread`; its snapshot's lock.
            unsafe { &*jt_ptr }.root_snapshot.lock().contains(&obj)
        };
        {
            let _jni = JniTls::context(&shared).with_thread(jt_ptr);
            let _flags = TransitionOverrides::set(Some(true), Some(true), None);
            push_local_frame(16);
            let env = get_jni_env();
            // A redefinition anywhere in the process between the deposit and a
            // re-entry makes that re-entry a full one (correctly); the
            // incremental assertions below hold only without one.
            let redefinitions = cratonvm_classloading::class_redefinition_count();
            // SAFETY: the test's own boxed `JvmThread`, not yet in native.
            unsafe { (*jt_ptr).redefinitions_seen = redefinitions };
            let call = JniNativeCall::enter(raw_local_escapes());
            assert!(d3k_in_native(jt_ptr));
            assert_eq!(appended(), Some(0), "a fresh call starts from its full deposit");

            let text = CString::new("d10j").unwrap();
            let s1 = jni_new_string_utf(env, text.as_ptr());
            let s2 = jni_new_string_utf(env, text.as_ptr());
            assert!(s1 != 0 && s2 != 0 && is_indirect_local_tag(s1) && is_indirect_local_tag(s2));
            assert!(d3k_in_native(jt_ptr), "back in native after each call");
            assert_eq!(
                JNI_NATIVE_CALL.with(|t| t.leaf.get()),
                LEAF_NONE,
                "the vm-only state is given back"
            );
            if cratonvm_classloading::class_redefinition_count() == redefinitions {
                assert!(
                    appended().is_some_and(|n| n >= 2),
                    "both re-entries appended instead of re-depositing: {:?}",
                    appended()
                );
            }
            assert!(in_snapshot(s1), "the first string is a root of the window");
            assert!(in_snapshot(s2), "the second string is a root of the window");

            // A raise reaches `with_jni_context`: the full way back in.
            assert_eq!(jni_new_int_array(env, -1), 0);
            let _ = take_jni_pending_exception();
            assert!(d3k_in_native(jt_ptr));
            assert_eq!(appended(), Some(0), "an escalated function re-deposits");

            // A fold into the thread since the deposit: the leave is not
            // quiet, and the re-entry is a full deposit with a fresh sync.
            // SAFETY: the test's own boxed `JvmThread`; only an atomic.
            unsafe { &*jt_ptr }
                .gc_block_state
                .blocked_folds
                .fetch_add(1, Ordering::AcqRel);
            let s3 = jni_new_string_utf(env, text.as_ptr());
            assert!(s3 != 0);
            assert!(d3k_in_native(jt_ptr));
            assert_eq!(appended(), Some(0), "a fold forces the full deposit");
            let synced = JNI_NATIVE_CALL.with(|t| {
                t.record.borrow().as_ref().is_some_and(|r| {
                    // SAFETY: see `NativeCallRecord::block_state`.
                    r.in_native && r.sync == NativeSync::read(&shared, unsafe { &*r.block_state })
                })
            });
            assert!(synced, "the full re-entry recorded a fresh sync point");
            assert!(in_snapshot(s3), "the full deposit roots the new local too");
            assert!(in_snapshot(s1), "and the older ones");

            drop(call);
            assert!(!d3k_in_native(jt_ptr), "the call's end leaves native");
            assert_eq!(
                shared.mem.gc_barrier.blocked_count(),
                0,
                "every in-native entry was released exactly once"
            );
        }
        shared.threads.thread_registry.mark_dead(tid);
    }

    /// gcd d10/j: the package rule of the three JNI switches.
    #[test]
    fn jni_switches_move_as_one_package_and_refuse_arm_c() {
        let s = jni_switches_from;
        let sw = |indirect_locals, foreign_transitions, native_transitions| JniSwitches {
            indirect_locals,
            foreign_transitions,
            native_transitions,
        };
        assert_eq!(s(None, None, None), sw(false, false, false), "the defaults: all off");
        assert_eq!(s(Some(true), None, None), sw(true, true, true), "the package switch");
        assert_eq!(
            s(Some(true), Some(false), None),
            sw(false, false, true),
            "an explicit off wins, and takes the foreign transitions with it"
        );
        assert_eq!(s(Some(true), None, Some(false)), sw(true, false, true));
        assert_eq!(s(None, Some(true), None), sw(true, false, false), "arm B");
        assert_eq!(s(None, Some(true), Some(true)), sw(true, true, false), "arm D");
        assert_eq!(s(None, None, Some(true)), sw(false, false, false), "arm C is refused");
        assert_eq!(s(Some(false), None, Some(true)), sw(false, false, false));
        assert_eq!(s(Some(false), Some(true), Some(true)), sw(true, true, false));

        assert_eq!(jni_switch_value(None), None);
        assert_eq!(jni_switch_value(Some("")), None, "empty is unset");
        assert_eq!(jni_switch_value(Some("  ")), None);
        for off in ["0", "false", "off", "no", " 0 "] {
            assert_eq!(jni_switch_value(Some(off)), Some(false), "{off:?}");
        }
        for on in ["1", "true", "on", "yes"] {
            assert_eq!(jni_switch_value(Some(on)), Some(true), "{on:?}");
        }
    }

    /// gcd d10/j: the in-place descriptor tags are exactly the parser's.
    #[test]
    fn param_type_tags_match_the_parser() {
        for d in [
            "()V",
            "(I)V",
            "(ILjava/lang/String;[BZ)V",
            "(JD[[I[Ljava/lang/Object;[[Ljava/lang/String;FSCB)J",
            "([J)Ljava/lang/Object;",
            "(Lx;Ly;)V",
            // Malformed: the same answer as the parser, whatever it is.
            "(",
            "",
            "(Lunterminated",
            "([",
            "([[",
            "(Q?I)V",
        ] {
            assert_eq!(
                param_type_tags(d).collect::<Vec<u8>>(),
                parse_param_types_inner(d),
                "{d:?}"
            );
        }
    }

    /// gcd d10/j: a closed implicit frame's storage is kept (emptied) for the
    /// next one, never a handle; an oversized frame is not kept.
    #[test]
    fn a_closed_local_frame_is_reused_empty() {
        let spare = || {
            JNI_NATIVE_CALL.with(|t| {
                let v = t.spare_local_frame.take();
                let seen = (v.len(), v.capacity());
                t.spare_local_frame.set(v);
                seen
            })
        };
        let _tls = JniTls::cleanup_only();
        let base = local_frame_depth();
        push_local_frame(16);
        JNI_LOCAL_FRAMES.with(|f| {
            if let Some(top) = f.borrow_mut().last_mut() {
                top.push(0x1000);
            }
        });
        truncate_local_frames(base);
        let (len, cap) = spare();
        assert_eq!(len, 0, "the kept storage holds no handle");
        assert!(cap >= 16, "the implicit frame's storage is kept");
        push_local_frame(16);
        let top_len = JNI_LOCAL_FRAMES.with(|f| f.borrow().last().map(Vec::len));
        assert_eq!(top_len, Some(0), "the reused frame starts empty");
        truncate_local_frames(base);

        // An oversized frame is freed, not kept.
        push_local_frame(SPARE_LOCAL_FRAME_MAX * 4);
        truncate_local_frames(base);
        let (_, cap) = spare();
        assert!(cap <= SPARE_LOCAL_FRAME_MAX, "no oversized storage kept: {cap}");
        assert_eq!(local_frame_depth(), base);
    }

    /// `gcd-d2i-foreign-attached-threads-publish-no-os-tid`: with
    /// `CRATONVM_JNI_FOREIGN_TRANSITIONS` on, an attached thread publishes its
    /// OS tid while it runs a JNI function (or Java) and withdraws it when it
    /// goes idle; with the flag off nothing is published, as before.
    #[test]
    fn an_attached_thread_publishes_its_os_tid_only_while_it_runs() {
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let vm = Arc::clone(&shared);
        let outcome = std::thread::spawn(move || -> Result<(), String> {
            let has_backend = cfg!(any(windows, target_os = "linux"));
            let registry = &vm.threads.thread_registry;
            // Flag on.
            let flags = TransitionOverrides::set(None, None, Some(true));
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("d3k-os-tid"));
            let tid = with_foreign_thread(|jt| jt.thread_id).ok_or("attached")?;
            let check = || -> Result<(), String> {
                if registry.os_tid_of(tid).is_some() {
                    return Err("an idle attachment publishes no OS tid".into());
                }
                let fx = ForeignJniEntry::enter();
                let running = registry.os_tid_of(tid);
                drop(fx);
                if has_backend && running.is_none() {
                    return Err("a running attachment publishes its OS tid".into());
                }
                if registry.os_tid_of(tid).is_some() {
                    return Err("idle again: withdrawn".into());
                }
                Ok(())
            };
            let on = check();
            w10c_detach(&vm);
            drop(flags);
            on?;
            // Flag off: a Java call's transition publishes nothing.
            let _flags = TransitionOverrides::set(None, None, Some(false));
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("d3k-os-tid-off"));
            let tid = with_foreign_thread(|jt| jt.thread_id).ok_or("attached")?;
            let guard = ForeignCallGuard::enter();
            let running = registry.os_tid_of(tid);
            drop(guard);
            w10c_detach(&vm);
            if running.is_some() {
                return Err("flag off: nothing published".into());
            }
            Ok(())
        })
        .join()
        .expect("the attached thread does not panic");
        outcome.expect("the OS tid follows the attachment's running state");
    }

    /// gen r5w1/crash5 (`ForeignThreadBox`): a host thread that EXITS attached
    /// -- no `DetachCurrentThread` -- is reaped by its thread-local destructor.
    /// The entry is dead and has left the barrier's blocked population. Before
    /// the fix the destructor freed the `JvmThread` and left the entry alive,
    /// idle-blocked and publishing `tlab_addr` / `jvm_thread_addr` into the
    /// freed allocation, which every later pause read
    /// (`collect_reserved_tlab_tails`).
    #[test]
    fn a_thread_that_exits_attached_is_reaped_not_left_dangling() {
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let blocked_before = shared.mem.gc_barrier.blocked_count();
        let reaped_before = crate::threading::thread_registry::root_write_repair_census().2;
        let vm = Arc::clone(&shared);
        let tid = std::thread::spawn(move || {
            attach_foreign_thread_idle(vm, false, Some("crash5-exits-attached"));
            with_foreign_thread(|jt| jt.thread_id)
            // No detach: the thread ends here and its destructors run.
        })
        .join()
        .expect("the attached thread exits cleanly")
        .expect("the thread was attached");
        let registry = &shared.threads.thread_registry;
        assert!(
            !registry.is_alive(tid),
            "an attachment whose OS thread exited is dead"
        );
        assert_eq!(registry.own_jvm_thread_addr(tid), None);
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "no TLAB tail is read through the exited attachment"
        );
        assert_eq!(
            shared.mem.gc_barrier.blocked_count(),
            blocked_before,
            "the exited attachment left the blocked population"
        );
        assert!(
            crate::threading::thread_registry::root_write_repair_census().2 > reaped_before,
            "reaped, not leaked"
        );
    }

    /// The reap is barrier-serialized like the detach it replaces: a thread
    /// that exits attached while another thread holds a pause is not marked
    /// dead (and its TLAB not touched) until the pause completes.
    #[test]
    fn a_thread_exiting_attached_waits_out_a_pause_before_it_is_reaped() {
        use std::sync::mpsc;
        use std::time::Duration;
        const WAIT: Duration = Duration::from_secs(30);
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let (to_main, from_foreign) = mpsc::channel::<ThreadId>();
        let (to_foreign, from_main) = mpsc::channel::<()>();
        let vm = Arc::clone(&shared);
        let foreign = std::thread::spawn(move || {
            attach_foreign_thread_idle(vm, false, Some("crash5-exit-in-pause"));
            if let Some(tid) = with_foreign_thread(|jt| jt.thread_id) {
                let _ = to_main.send(tid);
            }
            let _ = from_main.recv_timeout(WAIT);
            // Returns attached: the destructor reaps while main holds a pause.
        });
        let tid = from_foreign
            .recv_timeout(WAIT)
            .expect("the thread attached");
        let alive = shared.threads.thread_registry.alive_count() as u32;
        assert!(shared.mem.gc_barrier.request_stw(main_tid, alive));
        let mut pause = PauseHeld(&shared, false);
        shared.mem.gc_barrier.wait_for_all();
        to_foreign.send(()).expect("the foreign thread waits");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            shared.threads.thread_registry.is_alive(tid),
            "the reap must not run inside another thread's pause"
        );
        pause.complete();
        foreign.join().expect("the foreign thread exits cleanly");
        assert!(!shared.threads.thread_registry.is_alive(tid));
        drop(pause);
    }

    /// Function `n` (1-based, as `jvmti.h` numbers them) of a C `jvmtiEnv*`.
    #[cfg(feature = "experimental-debug")]
    fn jvmti_function(env: *mut std::ffi::c_void, n: usize) -> usize {
        // SAFETY: a `jvmtiEnv*` points at its function-table pointer, and the
        // table has more than `n` entries.
        unsafe { *(*(env as *const *const usize)).add(n - 1) }
    }

    /// `docs/internal/fixed-bugs/interpreter-c-jvmti-functions-decode-handles-without-the-foreign-jni-entry-FIXED-20260925.md`
    /// (interpreter round i1 wave 19), with the transitions and indirect
    /// locals on: an idle foreign agent thread holds a local `jthread` for a
    /// registered thread, two full collections run while it is idle, and its
    /// `SetEventNotificationMode(ENABLE, ClassPrepare, thread)` still finds
    /// the thread. The C JVMTI table decoded the handle GC-blocked, without
    /// the entry's leave (which remaps the attach-level frame), so a moved
    /// mirror answered `JVMTI_ERROR_INVALID_THREAD`.
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn an_idle_foreign_jvmti_call_decodes_its_thread_handle_after_a_collection() {
        use std::sync::mpsc;
        use std::time::Duration;
        type SetModeFn = extern "C" fn(*mut std::ffi::c_void, JInt, JInt, JObject) -> JInt;
        type DisposeFn = extern "C" fn(*mut std::ffi::c_void) -> JInt;
        const WAIT: Duration = Duration::from_secs(30);
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let mut main = JvmThread::new(main_tid, "w19-main");
        let (to_main, from_foreign) = mpsc::channel::<Result<(), String>>();
        let (to_foreign, from_main) = mpsc::channel::<()>();
        let vm = Arc::clone(&shared);
        let foreign = std::thread::spawn(move || {
            FOREIGN_TRANSITIONS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            INDIRECT_LOCALS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("w19-agent"));
            let env = get_jni_env();
            let setup = || -> Result<(*mut std::ffi::c_void, JObject), String> {
                let text = CString::new("w19 thread mirror stand-in").unwrap();
                let thread = jni_new_string_utf(env, text.as_ptr());
                if thread == 0 {
                    return Err("NewStringUTF answered NULL".into());
                }
                {
                    // Published as the main thread's mirror (the registry
                    // only roots, remaps and compares it) while a counted
                    // mutator.
                    let _fx = ForeignJniEntry::enter();
                    let obj = jobject_to_obj(thread).ok_or("the local decodes")?;
                    vm.threads
                        .thread_registry
                        .set_java_thread_obj(main_tid, obj);
                }
                let jvmti = crate::jvmti::native_env::new_env_for_version(0x3001_0200)
                    .ok_or("GetEnv for JVMTI 1.2")?;
                if !foreign_is_idle() {
                    return Err("the agent thread is idle before the collection".into());
                }
                Ok((jvmti, thread))
            };
            let handles = setup();
            let _ = to_main.send(handles.as_ref().map(|_| ()).map_err(String::clone));
            if handles.is_ok() && from_main.recv_timeout(WAIT).is_err() {
                w10c_detach(&vm);
                return Err("the collection never ran".to_string());
            }
            let result = handles.and_then(|(jvmti, thread)| {
                // SAFETY (both transmutes): the slot's `jvmti.h` signature.
                let set_mode =
                    unsafe { std::mem::transmute::<usize, SetModeFn>(jvmti_function(jvmti, 2)) };
                let dispose =
                    unsafe { std::mem::transmute::<usize, DisposeFn>(jvmti_function(jvmti, 127)) };
                // `JVMTI_ENABLE`, `JVMTI_EVENT_CLASS_PREPARE` (no capability).
                let code = set_mode(jvmti, 1, 56, thread);
                let idle = foreign_is_idle();
                let _ = dispose(jvmti);
                if code != 0 {
                    return Err(format!("SetEventNotificationMode answered {code}"));
                }
                if !idle {
                    return Err("the JVMTI function returns the thread to idle".into());
                }
                Ok(())
            });
            w10c_detach(&vm);
            result
        });
        from_foreign
            .recv_timeout(WAIT)
            .expect("the foreign thread reports")
            .expect("the idle agent thread's setup");
        let cycles = shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed);
        crate::runtime::interpreter::force_gc_for_vm(&shared, &mut main);
        crate::runtime::interpreter::force_gc_for_vm(&shared, &mut main);
        assert!(
            shared
                .mem
                .gc_cycle_count
                .load(std::sync::atomic::Ordering::Relaxed)
                > cycles,
            "a collection ran"
        );
        to_foreign.send(()).expect("the foreign thread waits");
        foreign
            .join()
            .expect("the foreign thread does not panic")
            .expect("the thread handle names the thread after the collection");
    }

    /// The same page's class functions: an idle foreign agent thread's
    /// `GetClassSignature` does not decode its `jclass` while another
    /// thread's stop-the-world pause holds; it waits the pause out, answers,
    /// and leaves the thread idle again.
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn an_idle_foreign_jvmti_class_function_waits_out_a_pause() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;
        type SignatureFn =
            extern "C" fn(*mut std::ffi::c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt;
        type DeallocateFn = extern "C" fn(*mut std::ffi::c_void, *mut u8) -> JInt;
        const WAIT: Duration = Duration::from_secs(30);
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let cid = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/W19JvmtiIdleAgent", 0)
            .expect("Compatible mode fabricates");
        let klass = class_id_to_jclass(cid);
        let (to_main, from_foreign) = mpsc::channel::<String>();
        let (to_foreign, from_main) = mpsc::channel::<()>();
        let returned = Arc::new(AtomicBool::new(false));
        let vm = Arc::clone(&shared);
        let done = Arc::clone(&returned);
        let foreign = std::thread::spawn(move || {
            FOREIGN_TRANSITIONS_TEST_OVERRIDE.with(|c| c.set(Some(true)));
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("w19-pause"));
            let jvmti = crate::jvmti::native_env::new_env_for_version(0x3001_0200);
            let _ = to_main.send("attached".to_string());
            if let (Some(jvmti), Ok(())) = (jvmti, from_main.recv_timeout(WAIT)) {
                // SAFETY (both transmutes): the slot's `jvmti.h` signature.
                let signature =
                    unsafe { std::mem::transmute::<usize, SignatureFn>(jvmti_function(jvmti, 48)) };
                let deallocate = unsafe {
                    std::mem::transmute::<usize, DeallocateFn>(jvmti_function(jvmti, 47))
                };
                let mut sig = std::ptr::null_mut::<u8>();
                let code = signature(jvmti, klass, &mut sig, std::ptr::null_mut());
                done.store(true, Ordering::SeqCst);
                let text = if code == 0 && !sig.is_null() {
                    // SAFETY: a NUL-terminated string the table allocated.
                    let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()) }
                        .to_string_lossy()
                        .into_owned();
                    let _ = deallocate(jvmti, sig);
                    text
                } else {
                    format!("error {code}")
                };
                let idle = if foreign_is_idle() { "idle" } else { "running" };
                let _ = to_main.send(format!("{text} {idle}"));
                let _ = from_main.recv_timeout(WAIT);
            }
            w10c_detach(&vm);
        });
        assert_eq!(from_foreign.recv_timeout(WAIT).as_deref(), Ok("attached"));
        let alive = shared.threads.thread_registry.alive_count() as u32;
        assert_eq!(alive, 2, "main and the attached thread");
        assert!(shared.mem.gc_barrier.request_stw(main_tid, alive));
        let mut pause = PauseHeld(&shared, false);
        shared.mem.gc_barrier.wait_for_all();
        to_foreign.send(()).expect("the foreign thread waits");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !returned.load(Ordering::SeqCst),
            "GetClassSignature must not decode its handle while another thread's pause holds"
        );
        pause.complete();
        assert_eq!(
            from_foreign.recv_timeout(WAIT).as_deref(),
            Ok("Lcratonvm/test/W19JvmtiIdleAgent; idle")
        );
        let _ = to_foreign.send(());
        foreign.join().expect("the foreign thread does not panic");
        drop(pause);
    }

    /// gc-common w10-c discovery: a foreign attachment's transitions use the
    /// attachment's OWN VM. They resolved `process_vm()`, the most recently
    /// created VM, so with a second VM published the outermost Java call left
    /// (and the return re-entered) the OTHER VM's blocked region.
    #[test]
    fn foreign_transitions_use_the_attachments_own_vm() {
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (a, _) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let (b, _) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let _raw = attach_foreign_thread(&a, false, None);
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, std::sync::atomic::Ordering::Release)
        });
        let _ = a.mem.gc_barrier.mark_blocked_region_enter();
        // The newest published VM is not this thread's.
        set_process_vm(&b);
        assert!(Arc::ptr_eq(&foreign_attachment_vm().expect("recorded"), &a));
        {
            let _fg = ForeignCallGuard::enter();
            assert_eq!(
                a.mem.gc_barrier.blocked_count(),
                0,
                "left A's blocked region"
            );
            assert_eq!(b.mem.gc_barrier.blocked_count(), 0, "B was never touched");
        }
        assert_eq!(
            a.mem.gc_barrier.blocked_count(),
            1,
            "re-entered A's blocked region"
        );
        assert_eq!(b.mem.gc_barrier.blocked_count(), 0);
        let tid = with_foreign_thread(|jt| jt.thread_id).unwrap();
        a.threads.thread_registry.mark_dead(tid);
        a.mem.gc_barrier.mark_blocked_region_leave();
        assert!(detach_foreign_thread(&a));
        assert!(
            FOREIGN_ATTACH_VM.with(|c| c.borrow().is_none()),
            "detach forgets the VM"
        );
    }

    /// gc-common w11-b (`common-w10c-aio-dispatcher-is-one-per-process`): the
    /// AIO dispatcher pool attaches to the VM its launch was for, resolved by
    /// identity, never to `process_vm()` (the newest VM). Outside a launch
    /// there is no VM to serve, and nothing is started or guessed.
    #[test]
    fn aio_dispatcher_resolves_the_launching_vm_not_the_newest() {
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (a, _) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let (b, _) = w10c_vm(crate::config::GcAlgorithm::Generational);
        set_process_vm(&a);
        // The newest published VM is B.
        set_process_vm(&b);
        let resolved = live_vm_by_identity(a.vm_identity).expect("A is live and published");
        assert!(Arc::ptr_eq(&resolved, &a), "A's launch must resolve A");
        assert!(Arc::ptr_eq(
            &live_vm_by_identity(b.vm_identity).expect("B is live"),
            &b
        ));
        assert!(live_vm_by_identity(usize::MAX).is_none());
        assert_eq!(cratonvm_native_io::async_socket::launching_vm(), None);
        // No launching VM: returns without spawning a thread.
        start_aio_dispatcher();
        drop(resolved);
        let a_identity = a.vm_identity;
        let a_weak = Arc::downgrade(&a);
        drop(a);
        if a_weak.upgrade().is_none() {
            assert!(
                live_vm_by_identity(a_identity).is_none(),
                "a dropped VM is never resolved"
            );
        }
    }

    /// `common-w9g-jni-element-copies-are-bound-to-the-getting-thread`: a
    /// `Get<Int>ArrayElements` / `GetStringChars` on one thread released on
    /// ANOTHER thread with its own JNI context on the same VM writes back,
    /// frees, and deletes the keep-alive global ref. A release through an
    /// unrelated VM's context finds nothing (and so frees nothing).
    #[test]
    fn element_and_string_copies_release_from_another_thread() {
        use crate::config::VmConfig;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let other = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "w10c-copies");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let env = get_jni_env();
        push_local_frame(16); // the native's implicit dispatch frame
        let globals = || shared.natives.jni_global_refs.lock().count();
        let outstanding = || shared.natives.jni_global_refs.lock().outstanding_copies();
        let globals_before = globals();

        let ints = jni_new_int_array(env, 4);
        jni_set_int_array_region(env, ints, 0, 4, [1, 2, 3, 4].as_ptr());
        let text = CString::new("w10-c").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        let elems = jni_get_int_array_elements(env, ints, std::ptr::null_mut());
        let chars = jni_get_string_chars(env, s, std::ptr::null_mut());
        assert!(!elems.is_null() && !chars.is_null());
        assert_eq!(
            globals(),
            globals_before + 1,
            "the element copy's keep-alive root"
        );
        assert_eq!(outstanding(), 2);
        unsafe {
            for (i, v) in [10, 20, 30, 40].into_iter().enumerate() {
                *elems.add(i) = v;
            }
        }
        let (e, c) = (elems as usize, chars as usize);

        // Another VM's context: not this VM's copies.
        let vm_b = Arc::clone(&other);
        std::thread::spawn(move || {
            let _jni = JniTls::context(&vm_b);
            let env = get_jni_env();
            jni_release_int_array_elements(env, 0, e as *mut JInt, 0);
            jni_release_string_chars(env, 0, c as *const JChar);
        })
        .join()
        .expect("the other VM's release does not panic");
        assert_eq!(outstanding(), 2, "an unrelated VM never sees these copies");
        assert_eq!(globals(), globals_before + 1);

        // This VM's context on a thread that did not Get them.
        let vm_a = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _jni = JniTls::context(&vm_a);
            let env = get_jni_env();
            jni_release_int_array_elements(env, 0, e as *mut JInt, 0);
            jni_release_string_chars(env, 0, c as *const JChar);
        })
        .join()
        .expect("the cross-thread release does not panic");
        assert_eq!(outstanding(), 0, "both records went with their releases");
        assert_eq!(
            globals(),
            globals_before,
            "the keep-alive global ref was deleted"
        );
        let mut out = [0 as JInt; 4];
        jni_get_int_array_region(env, ints, 0, 4, out.as_mut_ptr());
        assert_eq!(out, [10, 20, 30, 40], "the write-back landed");
        assert_eq!(take_jni_pending_exception(), None);
    }

    /// gc-common w10-c discovery: `PushLocalFrame`'s capacity is a reservation
    /// hint (`PushLocalFrame(env, INT_MAX)` reserved 16 GiB and aborted), and a
    /// negative capacity is refused with `JNI_ERR` and a pending exception, as
    /// HotSpot does, for `EnsureLocalCapacity` too.
    #[test]
    fn local_capacity_is_a_hint_and_a_negative_one_is_refused() {
        use crate::config::VmConfig;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "w10c-capacity");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let env = get_jni_env();
        let base = local_frame_depth();
        assert_eq!(jni_push_local_frame(env, JInt::MAX), JNI_OK);
        assert_eq!(local_frame_depth(), base + 1);
        assert_eq!(jni_pop_local_frame(env, 0), 0);
        assert_eq!(local_frame_depth(), base);
        assert_eq!(jni_exception_check(env), JNI_FALSE);

        assert_eq!(jni_push_local_frame(env, -1), JNI_ERR);
        assert_eq!(local_frame_depth(), base, "a refused push pushes nothing");
        assert_eq!(jni_exception_check(env), JNI_TRUE);
        jni_exception_clear(env);

        assert_eq!(jni_ensure_local_capacity(env, -5), JNI_ERR);
        assert_eq!(jni_exception_check(env), JNI_TRUE);
        jni_exception_clear(env);
        assert_eq!(jni_ensure_local_capacity(env, 1 << 20), JNI_OK);
        assert_eq!(jni_exception_check(env), JNI_FALSE);
    }

    /// `r12w8-compat2-jni-findclass-native-declaring-class-patch`: with no Java
    /// frame, `FindClass`'s context is the recorded native's declaring class,
    /// and nothing without one.
    #[test]
    fn find_class_context_answers_the_recorded_native_holder() {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "jni-findclass-holder");
        assert_eq!(jni_find_class_context(&shared, &mut thread), None);
        let cid = ClassId::new(7);
        thread.jni_native_class = Some(cid);
        assert_eq!(jni_find_class_context(&shared, &mut thread), Some(cid));
    }

    /// gcd d4/k (`docs/internal/gc/gcd-d3k-jni-exception-sentinel-escapes-to-native-FIXED-20260928.md`):
    /// the `u64::MAX` "no throwable could be built" sentinel is never handed to
    /// native code (`ExceptionOccurred` answers NULL while `ExceptionCheck`
    /// still answers true), and a build that failed for want of heap pends the
    /// VM's preallocated `OutOfMemoryError` instead, a real object.
    #[test]
    fn the_unbuilt_throwable_sentinel_never_reaches_native_code() {
        use crate::config::VmConfig;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "d4k-sentinel");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        let env = get_jni_env();
        push_local_frame(16);
        let saved = *shared.mem.singleton_oom.read();

        // Not a heap failure (a class that will not load): the sentinel.
        pend_unbuilt_throwable(false);
        assert_eq!(jni_exception_check(env), JNI_TRUE, "still flagged");
        assert_eq!(jni_exception_occurred(env), 0, "never handed out");
        jni_exception_clear(env);

        // A heap failure before the singleton exists: the sentinel too.
        *shared.mem.singleton_oom.write() = None;
        pend_unbuilt_throwable(true);
        assert_eq!(jni_exception_occurred(env), 0);
        jni_exception_clear(env);

        // A heap failure with the preallocated error: that object is pending.
        let text = CString::new("d4k stand-in").unwrap();
        let s = jni_new_string_utf(env, text.as_ptr());
        let stand_in = jobject_to_obj(s).expect("an object to stand in for the OOME");
        *shared.mem.singleton_oom.write() = Some(stand_in);
        pend_unbuilt_throwable(true);
        let t = jni_exception_occurred(env);
        assert_ne!(t, 0);
        assert_eq!(jobject_to_obj(t), Some(stand_in));
        jni_exception_clear(env);
        *shared.mem.singleton_oom.write() = saved;
    }

    /// gcd d4/k: the read-only local-frame iterator lane m's pin can call
    /// visits exactly the live locals of this thread's frames.
    #[test]
    fn for_each_live_local_ref_visits_the_live_locals() {
        use crate::config::VmConfig;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "d4k-locals");
        let _jni = JniTls::context(&shared).with_thread(&mut thread as *mut JvmThread);
        // Raw locals whatever the environment says (the default representation).
        let _flags = TransitionOverrides::set(None, Some(false), None);
        let env = get_jni_env();
        let before = {
            let mut n = 0usize;
            for_each_live_local_ref(|_| n += 1);
            n
        };
        push_local_frame(16);
        let a = CString::new("d4k a").unwrap();
        let b = CString::new("d4k b").unwrap();
        let sa = jni_new_string_utf(env, a.as_ptr());
        let sb = jni_new_string_utf(env, b.as_ptr());
        let oa = jobject_to_obj(sa).expect("a");
        let ob = jobject_to_obj(sb).expect("b");
        let mut seen = Vec::new();
        for_each_live_local_ref(|o| seen.push(o));
        assert_eq!(seen.len(), before + 2);
        assert!(seen.contains(&oa) && seen.contains(&ob));
        jni_delete_local_ref(env, sa);
        let mut after_delete = Vec::new();
        for_each_live_local_ref(|o| after_delete.push(o));
        assert!(!after_delete.contains(&oa), "a deleted local is not live");
        assert!(after_delete.contains(&ob));
        // Default flags: raw locals, so the pin must treat them as held raw.
        assert!(local_refs_may_be_held_raw());
    }

    /// Restores this thread's `raw_locals_counted` override on drop.
    struct RawLocalsCounted;

    impl RawLocalsCounted {
        fn set(on: bool) -> Self {
            JNI_NATIVE_CALL.with(|t| t.raw_locals_override.set(Some(on)));
            RawLocalsCounted
        }
    }

    impl Drop for RawLocalsCounted {
        fn drop(&mut self) {
            let _ = JNI_NATIVE_CALL.try_with(|t| t.raw_locals_override.set(None));
        }
    }

    /// Is the raw-JNI-locals count of `ledger`'s VM open?
    fn raw_locals_open_in(ledger: &Arc<cratonvm_gc::gc_quiescence::PauseLedger>) -> bool {
        cratonvm_gc::gc_quiescence::with_pause_ledger(
            ledger,
            cratonvm_gc::gc_quiescence::raw_jni_locals_open,
        )
    }

    /// gcd d4/k2 (lane m's request 1): a JNI native dispatched while its
    /// locals are raw (the default representation) is counted in its VM's
    /// pause ledger for the duration of the C call and not after it; with
    /// indirect locals it is not counted; and a raw escape inside such an
    /// uncounted call opens the count for good (fail closed).
    #[test]
    fn a_jni_dispatch_counts_its_raw_locals_in_the_pause_ledger() {
        use crate::config::VmConfig;
        extern "C" fn probe(_env: JNIEnv, _this: JObject) -> JInt {
            with_shared_vm(|s| raw_locals_open_in(s.mem.gc_barrier.pause_ledger()))
                .map_or(-1, |open| open as JInt)
        }
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let ledger = Arc::clone(shared.mem.gc_barrier.pause_ledger());
        let vm = Arc::clone(&shared);
        // A thread of its own: the fail-closed arm leaves thread-local state.
        let outcome = std::thread::spawn(move || -> Result<(), String> {
            let mut thread = JvmThread::new(ThreadId(0), "d4k2-raw-locals");
            let _jni = JniTls::context(&vm).with_thread(&mut thread as *mut JvmThread);
            let _counted = RawLocalsCounted::set(true);
            let env = get_jni_env();
            let fn_ptr = probe as *const () as usize;
            push_local_frame(16);
            let ledger = Arc::clone(vm.mem.gc_barrier.pause_ledger());

            // Raw locals: counted inside the call, not after it.
            let flags = TransitionOverrides::set(None, Some(false), None);
            if raw_locals_open_in(&ledger) {
                return Err("open before any call".into());
            }
            let r = unsafe { dispatch_jni_native(fn_ptr, env, 0, &[], "()I") };
            if r != Value::Int(1) {
                return Err(format!(
                    "raw locals: the native saw {r:?}, not an open count"
                ));
            }
            if raw_locals_open_in(&ledger) {
                return Err("the count must close with the call".into());
            }
            drop(flags);

            // Indirect locals: not counted.
            let flags = TransitionOverrides::set(None, Some(true), None);
            let r = unsafe { dispatch_jni_native(fn_ptr, env, 0, &[], "()I") };
            if r != Value::Int(0) {
                return Err(format!("indirect locals: the native saw {r:?}"));
            }
            // A raw escape inside an uncounted dispatch: open for good.
            let d = RawLocalsDispatch::enter();
            note_raw_local_escape();
            let late = raw_locals_open_in(&ledger);
            drop(d);
            drop(flags);
            if !late || !raw_locals_open_in(&ledger) {
                return Err("a raw escape in an uncounted call opens the count for good".into());
            }
            Ok(())
        })
        .join()
        .expect("the dispatching thread does not panic");
        outcome.expect("the raw-locals count follows the dispatch");
        // Only the fail-closed open is left, in this VM's ledger alone.
        assert!(raw_locals_open_in(&ledger));
        cratonvm_gc::gc_quiescence::note_raw_jni_locals_closed(&ledger);
        assert!(!raw_locals_open_in(&ledger));
    }

    /// gcd d4/k2: an attachment with raw locals is counted from attach to
    /// detach, and an OS thread that exits attached closes its count too.
    #[test]
    fn an_attachment_with_raw_locals_is_counted_until_it_detaches() {
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let (shared, _main_tid) = w10c_vm(crate::config::GcAlgorithm::Generational);
        let ledger = Arc::clone(shared.mem.gc_barrier.pause_ledger());
        let vm = Arc::clone(&shared);
        let outcome = std::thread::spawn(move || -> Result<(), String> {
            let _counted = RawLocalsCounted::set(true);
            let _flags = TransitionOverrides::set(None, None, Some(false));
            let ledger = Arc::clone(vm.mem.gc_barrier.pause_ledger());
            // Detached.
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("d4k2-attach"));
            count_attachment_raw_locals(Arc::clone(&ledger));
            let during = raw_locals_open_in(&ledger);
            w10c_detach(&vm);
            if !during || raw_locals_open_in(&ledger) {
                return Err(format!("detach: open during={during}, still open after"));
            }
            // Exiting attached: the box's drop closes it.
            attach_foreign_thread_idle(Arc::clone(&vm), false, Some("d4k2-exit"));
            count_attachment_raw_locals(Arc::clone(&ledger));
            let during = raw_locals_open_in(&ledger);
            let _ = FOREIGN_THREAD_BOX.try_with(|c| *c.borrow_mut() = None);
            clear_jni_thread();
            clear_jni_context();
            if !during || raw_locals_open_in(&ledger) {
                return Err(format!(
                    "exit attached: open during={during}, still open after"
                ));
            }
            Ok(())
        })
        .join()
        .expect("the attached thread does not panic");
        outcome.expect("the attachment's raw-locals count");
        assert!(!raw_locals_open_in(&ledger));
    }

    /// gcd d3/k: `DetachCurrentThread` from inside a JNI native method of a VM
    /// thread (Java frames below it) is refused with `JNI_ERR` and keeps the
    /// JNI context, as in HotSpot; it used to clear the context, and every
    /// later JNIEnv call of that native was answered NULL. Outside a native it
    /// still clears (the creating thread's detach before `DestroyJavaVM`).
    #[test]
    fn detach_from_inside_a_native_method_is_refused_and_keeps_the_context() {
        use crate::config::VmConfig;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "d3k-detach");
        let ptr: *mut JvmThread = &mut thread;
        let _jni = JniTls::context(&shared).with_thread(ptr);
        let vm: JavaVM = std::ptr::null();
        // SAFETY: the test's own thread, installed above; one field written.
        unsafe { (*ptr).jni_native_class = Some(ClassId::new(7)) };
        assert_eq!(jni_detach_current_thread(vm), JNI_ERR);
        assert!(with_jni_context(|_, _| ()).is_some(), "the context stays");
        // SAFETY: as above.
        unsafe { (*ptr).jni_native_class = None };
        assert_eq!(jni_detach_current_thread(vm), JNI_OK);
        assert!(
            with_shared_vm(|_| ()).is_none(),
            "outside a native it detaches"
        );
    }
}
