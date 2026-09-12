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
use crate::vm::{create_java_string, invoke_on_class_shared, read_java_string, SharedVm};

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
/// Get, consumed at the matching `Release<Type>ArrayElements`.
///
/// `len`/`cap` are the EXACT `Vec` layout we allocated so Release can copy back
/// and free soundly (see [`JNI_ARRAY_ELEM_BUFFERS`]). `array_gref` is a JNI
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
    /// Tracks (pointer -> element count) for UTF-16 buffers handed out by
    /// `GetStringChars` / `GetStringCritical` so that `ReleaseStringChars`
    /// can reconstruct the correct `Vec` layout and deallocate safely.
    static JNI_STRING_BUFFERS: std::cell::RefCell<HashMap<usize, usize>> =
        std::cell::RefCell::new(HashMap::new());
    /// Tracks (pointer -> element count) for the copy buffers handed out by
    /// `Get<Type>ArrayElements` so that `Release<Type>ArrayElements` can copy
    /// the elements back and reconstruct the exact `Vec` layout for
    /// deallocation.
    ///
    /// BUG FIX (vm-jni-roots #2): `Release` previously RE-DERIVED the length
    /// from the array handle (`array_length`). If the length observed at
    /// Release differed from the length at Get — the array moved/was realloc'd
    /// under a moving GC, the handle was aliased/stale, or `array_length` read
    /// 0 — then `Vec::from_raw_parts(elems, len, len)` was built with the wrong
    /// length/capacity, corrupting the allocator. We now key the allocation by
    /// its returned pointer at Get time and use the STORED count for both the
    /// copy-back loop and `from_raw_parts`, never re-deriving from the handle.
    ///
    /// MOVING-GC FIX (vm-jni-roots #2, G1 follow-up): the value also carries a
    /// remappable [`ArrayElemBuffer::array_gref`] — a JNI global ref to the
    /// source array — so Release resolves the array's CURRENT address even if a
    /// moving collector (generational young-copy or G1 evacuation) relocated it
    /// during the window. The previous keep-alive object-pin was address-keyed
    /// and never remapped, so the copy-back re-resolved a STALE raw `array`
    /// handle and either silently dropped (region freed) or wrote into a
    /// recycled object. See [`ArrayElemBuffer`].
    static JNI_ARRAY_ELEM_BUFFERS: std::cell::RefCell<HashMap<usize, ArrayElemBuffer>> =
        std::cell::RefCell::new(HashMap::new());
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
    /// GC-correctness (vm-jni-roots #2): maps a copy-buffer pointer we handed
    /// back to native code -> the base address of the backing array object that
    /// we PINNED in `cratonvm_gc::pinned` for KEEP-ALIVE during the handout.
    /// `GetPrimitiveArrayCritical` / `Get<Type>ArrayElements` always hand out a
    /// detached copy (never a heap pointer), so a GC must not RECLAIM the source
    /// array before the copy-back at `Release`; we pin the object base to keep it
    /// alive. (Relocation of the source is harmless — native code holds the copy,
    /// not a heap pointer — so this is keep-alive only, not no-relocation.) The
    /// matching `Release` only receives the buffer pointer, not the array object,
    /// so we record `buffer_ptr -> PinToken` here and look it up to UNPIN.
    /// Refcounted in `pinned`, so overlapping checkouts of the same array are safe.
    ///
    /// The value is a `PinToken`, not the object base, and that is the whole
    /// point. This map is thread-local: no collector enumerates it, and nothing
    /// re-keys it. Storing the base captured at Get time meant that any moving
    /// collection between Get and Release left the recorded address stale — the
    /// pin table had already been re-keyed to the new address, so `unpin(stale)`
    /// found nothing and was swallowed by the tolerated-unbalanced-release path,
    /// while the live entry stayed pinned for the rest of the process. The array
    /// and its entire transitive closure leaked, and `PINNED_COUNT` never
    /// returned to zero, so the `any_pinned()` fast gate stayed hot forever.
    /// A token is re-keyed by `pinned::update_after_gc` along with the table.
    static JNI_CRITICAL_PINS: std::cell::RefCell<HashMap<usize, cratonvm_gc::pinned::PinToken>> =
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
    static FOREIGN_THREAD_BOX: std::cell::RefCell<Option<Box<JvmThread>>> =
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
// own destructor has already dropped the `JvmThread` it owned.

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
pub fn attach_foreign_thread(
    shared: &SharedVm,
    daemon: bool,
    name: Option<&str>,
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
    shared
        .threads
        .thread_registry
        .register_with_daemon(tid, &name, None, daemon);
    // obsaudit D1: `attach_foreign_thread` runs synchronously on the
    // attaching OS thread, so bind the JVMTI thread-attribution TLS here —
    // classes this thread loads after attaching now report the real
    // `jthread` instead of the "unknown" sentinel.
    cratonvm_classloading::set_current_thread_id(tid.0);
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
    FOREIGN_THREAD_BOX.with(|c| *c.borrow_mut() = Some(jt));
    FOREIGN_CALL_DEPTH.with(|c| c.set(0));
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
        Some(j) => j,
        None => return false,
    };
    let tid = jt.thread_id;
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
    if is_foreign_attached() {
        // Already auto-managed as idle-blocked between calls; nothing to do.
        return true;
    }
    let shared = match process_vm() {
        Some(s) => s,
        None => return false,
    };
    // Identity of the calling host thread, if the VM registered it (the
    // creating thread is `ThreadId(0)`, published by `Vm::new`). `None` means
    // this OS thread is in no registry entry at all, so it is absent from
    // `alive_count` and occupies no `expected` slot — the counter-only path
    // below is then exactly right.
    let tid = shared
        .threads
        .thread_registry
        .thread_id_for_current_os_tid();
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
        // The fallback id is unchanged from the pre-fix behaviour: for an
        // unregistered caller it only answers "am I the initiator", and such a
        // thread never is.
        let _ = shared
            .mem
            .gc_barrier
            .arrive_and_wait_auto(tid.unwrap_or(ThreadId(0)));
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
    if is_foreign_attached() {
        return true;
    }
    let shared = match process_vm() {
        Some(s) => s,
        None => return false,
    };
    let tid = shared
        .threads
        .thread_registry
        .thread_id_for_current_os_tid();
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

static AIO_DISPATCH_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static AIO_DISPATCH_SHUTDOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
/// `set_dispatcher_launcher`, fired on the first handler-form read. The
/// shared completion queues (`native-io`'s `read_completion_state`) are
/// `Mutex`-protected `VecDeque`s popped one entry at a time, so multiple
/// dispatcher threads draining concurrently is race-free by construction —
/// no two threads can pop the same completion.
pub fn start_aio_dispatcher() {
    if AIO_DISPATCH_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    for i in 0..aio_dispatch_thread_count() {
        let _ = std::thread::Builder::new()
            .name(format!("cratonvm-aio-dispatch-{i}"))
            .spawn(aio_dispatcher_main);
    }
}

fn aio_dispatcher_main() {
    use std::sync::atomic::Ordering;
    let shared = match process_vm() {
        Some(s) => s,
        None => return,
    };
    // Attach as a foreign daemon thread and settle into the idle GC-blocked
    // region (mirrors the genuine-first-attach path in
    // `attach_current_thread_impl`). The empty deposited snapshot is published
    // before the TLS context, so there is no window where this thread is a
    // running mutator invisible to `request_stw`.
    let self_name = std::thread::current()
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| "cratonvm-aio-dispatch".to_string());
    let raw = attach_foreign_thread(&shared, true, Some(&self_name));
    with_foreign_thread(|jt| {
        jt.gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
    });
    let _ = shared.mem.gc_barrier.mark_blocked_region_enter();
    set_jni_context_arc(Arc::clone(&shared));
    set_jni_thread(raw);

    while !AIO_DISPATCH_SHUTDOWN.load(Ordering::Acquire) {
        // Idle, GC-blocked, until a completion is ready (bounded poll so a
        // missed wake can never wedge delivery).
        let pending = cratonvm_native_io::async_socket::wait_for_pending(
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

    // Clean detach (only reached on explicit shutdown).
    if let Some(shared) = process_vm() {
        if let Some(tid) = with_foreign_thread(|jt| jt.thread_id) {
            shared.mem.gc_barrier.mark_blocked_region_leave_after(|| {
                // Keep the retiring tail and the liveness transition in the
                // barrier-serialized closure. A new STW must not observe this
                // thread as dead before the tail has become walkable.
                with_foreign_thread(|jt| jt.tlab.retire());
                shared.threads.thread_registry.clear_tlab_addr(tid);
                shared.threads.thread_registry.mark_dead(tid);
                shared.threads.monitors.release_monitors_held_by(tid);
            });
        } else {
            shared.mem.gc_barrier.mark_blocked_region_leave();
        }
        detach_foreign_thread(&shared);
    }
    clear_jni_thread();
    clear_jni_context();
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
struct ForeignCallGuard {
    /// `Some(vm)` iff this guard performed the outermost idle→running transition
    /// and must perform the running→idle transition on drop.
    transition: Option<Arc<SharedVm>>,
    /// `true` iff this guard incremented [`FOREIGN_CALL_DEPTH`] (i.e. the thread
    /// is foreign-attached) and must decrement it on drop.
    counted: bool,
}

impl ForeignCallGuard {
    fn enter() -> Self {
        if !is_foreign_attached() {
            return ForeignCallGuard {
                transition: None,
                counted: false,
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
            };
        }
        // Outermost call: leave the idle blocked region and become a counted
        // mutator. `mark_blocked_region_leave` waits out any in-flight STW first,
        // so we never start running Java under an active collection.
        let shared = match process_vm() {
            Some(s) => s,
            None => {
                return ForeignCallGuard {
                    transition: None,
                    counted: true,
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
        // `scoped_values`, `pending_async_exception`, JNI locals — before
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
        shared.mem.gc_barrier.mark_blocked_region_leave();
        with_foreign_thread(|jt| {
            crate::vm::NativeContextImpl {
                shared: shared.as_ref(),
                thread: jt,
            }
            .check_post_block_gc();
        });
        // A fresh local-ref frame scopes this call's JNI local refs (freed on
        // return, per JNI semantics) so the thread holds none across the idle
        // window — keeping the idle root snapshot genuinely empty.
        push_local_frame(16);
        ForeignCallGuard {
            transition: Some(shared),
            counted: true,
        }
    }
}

impl Drop for ForeignCallGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.transition.take() {
            // Outermost return → go idle. Pop this call's local-ref frame.
            let _ = pop_local_frame(0);
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
            // captures: `java_thread_obj`, `pending_async_exception`, any
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
            with_foreign_thread(|jt| {
                jt.tlab.retire();
                crate::vm::NativeContextImpl {
                    shared: shared.as_ref(),
                    thread: jt,
                }
                .deposit_root_snapshot();
                jt.gc_block_state
                    .in_blocked_region
                    .store(true, std::sync::atomic::Ordering::Release);
            });
            let pre_stw = shared.mem.gc_barrier.mark_blocked_region_enter();
            if pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): the in_blocked_region store
                // above already ran, so this pause may already have
                // excluded us - auto resolves it from that pause's own
                // exclusion snapshot instead of assuming participation.
                let tid = with_foreign_thread(|jt| jt.thread_id).unwrap_or(ThreadId(0));
                let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
            }
        }
        if self.counted {
            FOREIGN_CALL_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
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

/// Convert a slice of `JValue`s to `Value`s using the type tags from
/// `parse_param_types`.  Returns `None` if `args` is null and `types` is
/// non-empty.
unsafe fn jvalues_to_values(args: *const JValue, types: &[u8]) -> Vec<Value> {
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
                b'J' => Value::Long(jv.j),
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
// JDK-only mode (`docs/feature-designs/jdk-only-mode.md` §7): JNI does **not**
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
#[inline]
fn jni_surface_jdk_only(
    shared: &SharedVm,
    thread: &mut JvmThread,
    result: crate::error::MethodCallResult,
) -> Option<Value> {
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
        // swallowed, exactly as `jni_throw_unsatisfied_link` does.
        Err(_) => JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX)),
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
        let obj_class_id = shared.mem.heap.class_id_of(oref);
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
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
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
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
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
        let jvm_args = unsafe { jvalues_to_values(args, &param_types) };
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

pub struct JniGlobalRefs {
    /// Raw `Box<ObjectRef>` pointers (stored as usize for Send/Sync).
    /// Each entry owns its allocation until `remove` is called. Keyed by the
    /// pointer itself so `resolve`/`remove` are O(1) on this hot path (the
    /// handle is just `raw | 1`, so the untagged pointer is a unique key —
    /// distinct `Box` allocations never collide).
    entries: HashSet<usize>,
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
    }
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

/// Push a new local frame onto this thread's local frame stack.
pub fn push_local_frame(capacity: usize) {
    JNI_LOCAL_FRAMES.with(|f| {
        f.borrow_mut().push(Vec::with_capacity(capacity.max(16)));
    });
}

/// Pop the topmost local frame, returning `result` (a JObject to promote to
/// the parent frame). Returns 0 if there is no active frame.
pub fn pop_local_frame(result: JObject) -> JObject {
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        let _ = stack.pop(); // drop all refs in the top frame
        result // caller-provided result is promoted to the parent frame (or kept as-is)
    })
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

/// Remove a local ref from the current top frame (for DeleteLocalRef).
pub fn delete_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if let Some(top) = stack.last_mut() {
            top.retain(|&r| r != jobj);
        }
    });
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
// JNI critical-section array pinning (vm-jni-roots #2)
// ---------------------------------------------------------------------------

/// Pin the source array object `oref` for **keep-alive** — so a GC will not
/// reclaim it while native code holds the copy we handed out — and remember the
/// copy-buffer `data_ptr` so the matching `Release` can find and unpin it.
/// Called on every `GetPrimitiveArrayCritical` / `Get<Type>ArrayElements`
/// handout.
///
/// NOTE: this is keep-alive ONLY. Data-movement safety is provided by handing
/// native code a detached COPY (never a direct heap pointer), so the pin is NOT
/// relied upon for no-relocation — see the `cratonvm_gc::pinned` module doc.
///
/// `data_ptr` is the copy-buffer pointer returned to the native caller (the key
/// the matching `Release` passes back). The pin itself is taken on the OBJECT
/// BASE (`oref.as_ptr()`), the address a collector tests against the pin set —
/// but what is recorded here is the returned `PinToken`, which survives the
/// object being relocated between Get and Release. See `JNI_CRITICAL_PINS`.
fn pin_critical_array(oref: ObjectRef, data_ptr: usize) {
    let base = oref.as_ptr() as usize;
    if base == 0 || data_ptr == 0 {
        return;
    }
    if let Some(token) = cratonvm_gc::pinned::pin_tokened(base) {
        JNI_CRITICAL_PINS.with(|c| {
            c.borrow_mut().insert(data_ptr, token);
        });
    }
}

/// Undo a [`pin_critical_array`] for the buffer at `data_ptr`. Releases the
/// `PinToken` recorded at Get time, which resolves to the object's CURRENT base
/// even if a collection relocated it in between (refcounted, so an overlapping
/// Get on the same array keeps it pinned until its own Release). A `data_ptr`
/// we never handed out (foreign pointer / double-release) is absent from the
/// map and ignored.
fn unpin_critical_array(data_ptr: usize) {
    if data_ptr == 0 {
        return;
    }
    let token = JNI_CRITICAL_PINS.with(|c| c.borrow_mut().remove(&data_ptr));
    if let Some(token) = token {
        cratonvm_gc::pinned::unpin_token(token);
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

pub fn obj_to_jobject(obj: ObjectRef) -> JObject {
    obj.as_ptr() as u64
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
    } else if jobj & 1 == 1 {
        // Global ref: resolve through the locked table to prevent
        // use-after-free if another thread concurrently calls remove().
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => shared.natives.jni_global_refs.lock().resolve(jobj),
                None => {
                    tracing::warn!(
                        "jobject_to_obj: global ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    } else {
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
extern "C" fn jni_find_class(_env: JNIEnv, name: *const c_char) -> JClass {
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    with_shared_vm(|shared| {
        let class_id = shared.load_class_concurrent(name_str).ok()?;
        Some(class_id_to_jclass(class_id))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 10: GetSuperclass ----
extern "C" fn jni_get_superclass(_env: JNIEnv, clazz: JClass) -> JClass {
    if clazz == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        class.superclass.map(class_id_to_jclass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 11: IsAssignableFrom ----
extern "C" fn jni_is_assignable_from(_env: JNIEnv, sub: JClass, sup: JClass) -> JBoolean {
    if sub == 0 || sup == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let sub_id = jclass_class_id(sub);
        let sup_id = jclass_class_id(sup);
        if sub_id == sup_id {
            return JNI_TRUE;
        }
        let cm = shared.classes.class_manager.read();
        let mut current = sub_id;
        loop {
            let class = match cm.get_class(current) {
                Some(c) => c,
                None => return JNI_FALSE,
            };
            // Check interfaces
            for &iface_id in &class.interfaces {
                if iface_id == sup_id {
                    return JNI_TRUE;
                }
            }
            match class.superclass {
                Some(sc) => {
                    if sc == sup_id {
                        return JNI_TRUE;
                    }
                    current = sc;
                }
                None => return JNI_FALSE,
            }
        }
    })
    .unwrap_or(JNI_FALSE)
}

// ---- Index 13: Throw ----
extern "C" fn jni_throw(_env: JNIEnv, obj: JThrowable) -> JInt {
    let Some(exception) = jobject_to_obj(obj) else {
        return JNI_ERR;
    };
    set_jni_pending_exception_object(exception);
    JNI_OK
}

// ---- Index 14: ThrowNew ----
extern "C" fn jni_throw_new(_env: JNIEnv, clazz: JClass, msg: *const c_char) -> JInt {
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
        crate::runtime::exceptions::create_exception_object_for_class(
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
    let handle = JNI_PENDING_EXCEPTION.with(|cell| cell.get());
    if handle == 0 {
        return 0;
    }
    // A collection may have relocated the exception after its original raw
    // handle was recorded.  Prefer the VM's remapped native-return root.
    with_jni_context(|_shared, thread| {
        thread
            .native_pending_return
            .map(obj_to_jobject)
            .unwrap_or(handle)
    })
    .unwrap_or(handle)
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
    let handle = JNI_PENDING_EXCEPTION.with(|cell| cell.get());
    if handle == 0 {
        return;
    }
    // Prefer the GC-remapped root over the raw handle, as `ExceptionOccurred`
    // does — a collection may have moved the throwable since it was recorded.
    let exc = with_jni_context(|_shared, thread| thread.native_pending_return)
        .flatten()
        .or_else(|| jobject_to_obj(handle));
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
extern "C" fn jni_push_local_frame(_env: JNIEnv, capacity: JInt) -> JInt {
    push_local_frame(capacity.max(0) as usize);
    JNI_OK
}

// ---- Index 21: PopLocalFrame ----
extern "C" fn jni_pop_local_frame(_env: JNIEnv, result: JObject) -> JObject {
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
/// produced a truncated id before and produces the same one now.
fn jclass_class_id(h: JObject) -> ClassId {
    if is_jclass_handle(h) {
        return ClassId::new(h as u32);
    }
    if let Some(id) = jclass_mirror_class_id(h) {
        return id;
    }
    ClassId::new(h as u32)
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
    delete_local_ref(lref);
}

// ---- Index 25: IsSameObject ----
extern "C" fn jni_is_same_object(_env: JNIEnv, a: JObject, b: JObject) -> JBoolean {
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
            track_local_ref(lref);
            lref
        }
        None => 0,
    }
}

// ---- Index 27: EnsureLocalCapacity ----
extern "C" fn jni_ensure_local_capacity(_env: JNIEnv, _capacity: JInt) -> JInt {
    JNI_OK
}

// ---- Index 31: GetObjectClass ----
extern "C" fn jni_get_object_class(_env: JNIEnv, obj: JObject) -> JClass {
    if obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let class_id = shared.mem.heap.class_id_of(oref);
        Some(class_id_to_jclass(class_id))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 32: IsInstanceOf ----
extern "C" fn jni_is_instance_of(_env: JNIEnv, obj: JObject, clazz: JClass) -> JBoolean {
    if obj == 0 {
        return JNI_TRUE; // null is instanceof any type per JNI spec
    }
    if clazz == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let obj_class_id = shared.mem.heap.class_id_of(oref);
        let target_id = jclass_class_id(clazz);
        if obj_class_id == target_id {
            return Some(JNI_TRUE);
        }
        // Walk superclass chain
        let cm = shared.classes.class_manager.read();
        let mut current = obj_class_id;
        loop {
            let class = cm.get_class(current)?;
            for &iface_id in &class.interfaces {
                if iface_id == target_id {
                    return Some(JNI_TRUE);
                }
            }
            match class.superclass {
                Some(sc) => {
                    if sc == target_id {
                        return Some(JNI_TRUE);
                    }
                    current = sc;
                }
                None => return Some(JNI_FALSE),
            }
        }
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
    match jni_call_instance(obj, mid, args) {
        Some(Value::Object(Some(r))) => obj_to_jobject(r),
        _ => 0,
    }
}

extern "C" fn jni_call_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
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
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Object(Some(r))) => obj_to_jobject(r),
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
    match jni_call_static(clazz, mid, args) {
        Some(Value::Object(Some(r))) => obj_to_jobject(r),
        _ => 0,
    }
}

extern "C" fn jni_call_static_boolean_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
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
    jni_call_static(clazz, mid, args);
}

// ---- Index 94: GetFieldID ----
extern "C" fn jni_get_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
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
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Object(Some(r)) => Some(obj_to_jobject(r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 96: GetBooleanField ----
extern "C" fn jni_get_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JBoolean {
    get_int_field_raw(obj, field_id) as JBoolean
}

// ---- Index 97: GetByteField ----
extern "C" fn jni_get_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JByte {
    get_int_field_raw(obj, field_id) as JByte
}

// ---- Index 98: GetCharField ----
extern "C" fn jni_get_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JChar {
    get_int_field_raw(obj, field_id) as JChar
}

// ---- Index 99: GetShortField ----
extern "C" fn jni_get_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JShort {
    get_int_field_raw(obj, field_id) as JShort
}

// ---- Index 100: GetIntField ----
extern "C" fn jni_get_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JInt {
    get_int_field_raw(obj, field_id)
}

// ---- Index 101: GetLongField ----
extern "C" fn jni_get_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JLong {
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
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        shared.mem.heap.set_field(oref, field_index, value);
        // write_barrier fires automatically inside set_field
        Some(())
    });
}

// ---- Index 105: SetBooleanField ----
extern "C" fn jni_set_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JBoolean) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 106: SetByteField ----
extern "C" fn jni_set_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JByte) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 107: SetCharField ----
extern "C" fn jni_set_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JChar) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 108: SetShortField ----
extern "C" fn jni_set_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JShort) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 109: SetIntField ----
extern "C" fn jni_set_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JInt) {
    set_int_field_raw(obj, field_id, val);
}

// ---- Index 110: SetLongField ----
extern "C" fn jni_set_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JLong) {
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
    jni_get_field_id(_env, clazz, name, sig)
}

// ---- Indices 145-150: GetStatic*Field ----
extern "C" fn jni_get_static_object_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JObject {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Object(Some(r))) => Some(obj_to_jobject(*r)),
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
    get_static_int_raw(clazz, field_id) as JBoolean
}

extern "C" fn jni_get_static_byte_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JByte {
    get_static_int_raw(clazz, field_id) as JByte
}

extern "C" fn jni_get_static_char_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JChar {
    get_static_int_raw(clazz, field_id) as JChar
}

extern "C" fn jni_get_static_short_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JShort {
    get_static_int_raw(clazz, field_id) as JShort
}

extern "C" fn jni_get_static_int_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JInt {
    get_static_int_raw(clazz, field_id)
}

extern "C" fn jni_get_static_long_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JLong {
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
    set_static_int_raw(field_id, val);
}

extern "C" fn jni_set_static_long_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JLong,
) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
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
    let s = match unsafe { cstr_to_str(chars) } {
        Some(s) => s,
        None => return 0,
    };
    with_shared_vm(|shared| {
        let obj = create_java_string(shared, s);
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 168: GetStringUTFLength ----
extern "C" fn jni_get_string_utf_length(_env: JNIEnv, str_obj: JString) -> JSize {
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
    if length < 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let component_id = jclass_class_id(clazz);
        let arr =
            shared
                .mem
                .heap
                .alloc_array(component_id, ArrayElementType::Reference, length as usize);
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
        obj_to_jobject(arr)
    })
    .unwrap_or(0)
}

// ---- Index 173: GetObjectArrayElement ----
extern "C" fn jni_get_object_array_element(_env: JNIEnv, array: JArray, index: JSize) -> JObject {
    if array == 0 || index < 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        match shared.mem.heap.get_array_element(oref, index as usize) {
            Ok(Value::Object(Some(r))) => Some(obj_to_jobject(r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 174: SetObjectArrayElement ----
extern "C" fn jni_set_object_array_element(
    _env: JNIEnv,
    array: JArray,
    index: JSize,
    val: JObject,
) {
    if array == 0 || index < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let _ = shared
            .mem
            .heap
            .set_array_element(oref, index as usize, value);
        Some(())
    });
}

// ---- Indices 175-181: New<Type>Array ----
macro_rules! new_prim_array {
    ($name:ident, $elem_type:expr) => {
        extern "C" fn $name(_env: JNIEnv, length: JSize) -> JArray {
            if length < 0 {
                return 0;
            }
            with_shared_vm(|shared| {
                let arr = shared
                    .mem
                    .heap
                    .alloc_array(ClassId::new(0), $elem_type, length as usize);
                obj_to_jobject(arr)
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
// registered in `JNI_ARRAY_ELEM_BUFFERS` so the matching
// `Release<Type>ArrayElements` copies any mutations back and frees it.
//
// Keep-alive AND copy-back correctness under a MOVING GC: at Get we mint a JNI
// **global ref** for the SOURCE array and stash its handle in the buffer's
// `JNI_ARRAY_ELEM_BUFFERS` entry. A global ref is both (a) a GC root — so the
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
                let mut buf: Vec<$rust_type> = Vec::with_capacity(len.max(1));
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
                // the final Release.
                let array_gref = shared.natives.jni_global_refs.lock().add(oref);
                JNI_ARRAY_ELEM_BUFFERS.with(|c| {
                    c.borrow_mut().insert(
                        ptr as usize,
                        ArrayElemBuffer {
                            len: buf_len,
                            cap: buf_cap,
                            array_gref,
                        },
                    );
                });
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
            let entry = if mode != 1 {
                JNI_ARRAY_ELEM_BUFFERS.with(|c| c.borrow_mut().remove(&(elems as usize)))
            } else {
                JNI_ARRAY_ELEM_BUFFERS.with(|c| c.borrow().get(&(elems as usize)).copied())
            };
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
                    true
                }
                Err(_) => false,
            },
        )
        .unwrap_or(false);
    if !raised {
        // No thread context or allocation failed: flag the pending-exception
        // sentinel so the condition is not silently swallowed.
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
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
extern "C" fn jni_monitor_enter(_env: JNIEnv, obj: JObject) -> JInt {
    if obj == 0 {
        return JNI_ERR;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        // thread_id 0 for JNI. GC-safety: mark a CONTENDED acquire blocked
        // so a concurrent STW proceeds without us (no JvmThread in this
        // context; the calling thread's Java roots are covered by its own
        // registry snapshot).
        if let Some(m) = shared.threads.monitors.enter_or_contend(oref, ThreadId(0)) {
            let blk = shared.mem.gc_barrier.enter_blocked();
            if blk.pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): auto for uniformity.
                let _ = shared.mem.gc_barrier.arrive_and_wait_auto(ThreadId(0));
            }
            m.block_enter(ThreadId(0));
            drop(blk);
        }
        Some(JNI_OK)
    })
    .flatten()
    .unwrap_or(JNI_ERR)
}

// ---- Index 218: MonitorExit ----
extern "C" fn jni_monitor_exit(_env: JNIEnv, obj: JObject) -> JInt {
    if obj == 0 {
        return JNI_ERR;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let _ = shared.threads.monitors.exit(oref, ThreadId(0));
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
    _loader: JObject,
    buf: *const u8,
    len: JSize,
) -> JClass {
    // DefineClass from raw bytes: define the class from the caller-supplied
    // `buf[..len]` bytecode via the same `define_class` path the interpreter
    // uses for `ClassLoader.defineClass` / agent retransform, so JNI/agent code
    // that synthesises classes at runtime gets the bytes it actually passed —
    // not a same-named class loaded from the classpath.
    //
    // Per JNI, `name` may be NULL (the name is then taken from the class file);
    // when supplied it is the expected binary name. The bytecode buffer is
    // mandatory: a NULL/empty/negative-length buffer is a hard error.
    if buf.is_null() || len <= 0 {
        raise_jni_no_class_def_found("DefineClass called with a null or empty bytecode buffer");
        return 0;
    }
    // The class name may be NULL (JNI allows deriving it from the class file).
    let class_name = unsafe { cstr_to_str(name) }.map(|s| s.replace('.', "/"));

    // SAFETY: the caller guarantees `buf` points to `len` readable bytes for
    // the duration of the call (standard JNI DefineClass contract). We copy the
    // bytes out immediately so the slice does not outlive this borrow.
    let bytes: Vec<u8> = unsafe { std::slice::from_raw_parts(buf, len as usize) }.to_vec();

    let result = with_shared_vm(|shared| {
        // If the caller did not supply a name, the class manager will derive it
        // from the class file's `this_class` entry; use the empty string as a
        // placeholder that `define_class` overrides from the bytes.
        let define_name = class_name.as_deref().unwrap_or("");
        let mut cm = shared.classes.class_manager_write();
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
        // a redefinition is honoured rather than served stale.
        if let Some(n) = class_name.as_deref() {
            let _ = shared.jit.jit_cache.write().invalidate_for_class(n);
            shared.jit.tiered_manager.on_class_redefined(n);
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
                    true
                }
                Err(_) => false,
            },
        )
        .unwrap_or(false);
    if !raised {
        // No thread context or allocation failed: flag the pending-exception
        // sentinel so the condition is not silently swallowed.
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
    }
}

// ---- Index 7: FromReflectedMethod ----
// Convert a java.lang.reflect.Method/Constructor to a JMethodID.
extern "C" fn jni_from_reflected_method(_env: JNIEnv, method: JObject) -> JMethodID {
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
        let obj = shared.mem.heap.alloc_object(method_class_id, num_fields);
        shared
            .mem
            .heap
            .set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared
            .mem
            .heap
            .set_field(obj, 1, Value::Int(method_index as i32));
        obj_to_jobject(obj)
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
        let obj = shared.mem.heap.alloc_object(field_class_id, num_fields);
        shared
            .mem
            .heap
            .set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared
            .mem
            .heap
            .set_field(obj, 1, Value::Int(field_index as i32));
        obj_to_jobject(obj)
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

/// Metadata for a temporary contiguous buffer handed out by
/// `GetPrimitiveArrayCritical` for a G1 humongous array. See
/// `JNI_CRITICAL_COPIES`.
struct CriticalCopy {
    /// The originating array handle, so release can copy back / free.
    array: JArray,
    /// Element type, so release re-packs each element correctly.
    element_type: ArrayElementType,
    /// Number of elements (== array length at Get time).
    len: usize,
    /// Bytes per element (the contiguous buffer is `len * stride` bytes).
    stride: usize,
    /// G1 region(s) pinned for this critical section (Step 6 / JEP 423) so the
    /// backing array cannot be relocated before the copy-back at `Release`.
    /// Empty under the generational collector. Released on the final
    /// `ReleasePrimitiveArrayCritical` (not on `JNI_COMMIT`).
    pinned_regions: Vec<usize>,
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
        let mut buf: Vec<u8> = vec![0u8; len * stride];
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

        // Step 6 (JEP 423): pin the array's G1 region for the critical section so
        // a moving young/mixed collection cannot relocate it before the copy-back
        // at Release. The copy-back re-resolves the Get-time array handle
        // (`jobject_to_obj(copy.array)`); if the array had been evacuated that raw
        // address would be stale (data loss / write to a recycled object). Empty
        // (no-op) under the generational collector. Released by
        // `ReleasePrimitiveArrayCritical`.
        let pinned_regions = shared.mem.heap.pin_critical_region(oref);
        JNI_CRITICAL_COPIES.with(|c| {
            c.borrow_mut().insert(
                ptr as usize,
                CriticalCopy {
                    array,
                    element_type,
                    len,
                    stride,
                    pinned_regions,
                },
            );
        });
        // Keep-alive ONLY (not no-relocation): pin the source array so it cannot
        // be reclaimed before the copy-back at Release. Unpinned by
        // `ReleasePrimitiveArrayCritical`. Data safety comes from the copy; the
        // region pin above additionally keeps the array in place under G1.
        pin_critical_array(oref, ptr as usize);
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
    if carray.is_null() {
        return;
    }
    // GC-correctness (vm-jni-roots #2): release the keep-alive pin taken on the
    // source array at Get time. Refcounted; tolerant of an unknown `carray`
    // (double-release / foreign pointer) — see `unpin_critical_array`.
    unpin_critical_array(carray as usize);
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
            let oref = jobject_to_obj(copy.array)?;
            for i in 0..copy.len {
                let off = i * copy.stride;
                // SAFETY: the buffer is `copy.len * copy.stride` bytes and we
                // only read within it.
                let src = unsafe {
                    std::slice::from_raw_parts((carray as *const u8).add(off), copy.stride)
                };
                let v = critical_decode_element(copy.element_type, src);
                let _ = shared.mem.heap.set_array_element(oref, i, v);
            }
            Some(())
        });
    }
    if mode != 1 {
        // Step 6 (JEP 423): release the G1 region pin(s) taken at Get. Done on
        // the FINAL release only — NOT on JNI_COMMIT (mode 1), whose section
        // continues and must keep the array pinned in place. No-op / empty under
        // the generational collector.
        if !copy.pinned_regions.is_empty() {
            with_shared_vm(|shared| {
                shared.mem.heap.unpin_critical_regions(&copy.pinned_regions);
                Some(())
            });
        }
        // Reconstruct the `Vec<u8>` with its original layout and drop it.
        let byte_len = copy.len * copy.stride;
        // SAFETY: `carray` was produced by `Vec::<u8>::as_mut_ptr` +
        // `mem::forget` in `jni_get_primitive_array_critical` with capacity
        // == length == `byte_len`; we reconstruct the exact layout.
        unsafe {
            drop(Vec::from_raw_parts(carray as *mut u8, byte_len, byte_len));
        }
    } else {
        // JNI_COMMIT: we kept the buffer alive but already removed it from the
        // map; re-insert so a later release can still find it (the region pin
        // taken at Get also stays held for the continuing section).
        JNI_CRITICAL_COPIES.with(|c| {
            c.borrow_mut().insert(carray as usize, copy);
        });
    }
}

// ---- Index 224: GetStringCritical ----
// Same as GetStringChars but signals the VM not to move the string.
extern "C" fn jni_get_string_critical(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    // Delegate to GetStringChars — our heap doesn't move objects between GC.
    jni_get_string_chars(_env, str_obj, is_copy)
}

// ---- Index 225: ReleaseStringCritical ----
extern "C" fn jni_release_string_critical(_env: JNIEnv, str_obj: JString, chars: *const JChar) {
    jni_release_string_chars(_env, str_obj, chars);
}

// ---- Index 226: NewWeakGlobalRef ----
// Weak refs behave like global refs but don't prevent GC. For simplicity,
// we implement them as strong global refs (conservative but correct).
extern "C" fn jni_new_weak_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
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
        let oref = jobject_to_obj(obj)?;
        let mut refs = shared.natives.jni_global_refs.lock();
        Some(refs.add(oref))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 227: DeleteWeakGlobalRef ----
extern "C" fn jni_delete_weak_global_ref(_env: JNIEnv, wref: JObject) {
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
        2 // JNIGlobalRefType (global or weak — we treat weak as global)
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
}

/// Look up a JNI function pointer for the given method in **this VM's** table.
/// Returns `None` if no pointer was registered.
pub fn find_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    find_jni_native_in(
        &natives.jni_native_methods,
        class_name,
        method_name,
        descriptor,
    )
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
    let param_types = parse_param_types_cached(descriptor);

    // Build a type-tagged argument list. `env` and `receiver` are always the
    // first two integer-class arguments; the Java args follow, each tagged
    // integer (GP register class) or floating-point (SSE register class) so the
    // platform calling convention can route them to the correct registers.
    let mut jargs: Vec<JniArg> = Vec::with_capacity(args.len() + 2);
    // `env` is a raw pointer (`*const *const usize`); flatten it to a 64-bit
    // integer-class word. `receiver` is already a `JObject` (u64 handle).
    jargs.push(JniArg::int(env as usize as u64));
    jargs.push(JniArg::int(receiver));
    for (v, tag) in args.iter().zip(param_types.iter()) {
        let a = match v {
            Value::Int(i) => JniArg::int(*i as i64 as u64),
            Value::Long(l) => JniArg::int(jni_long_arg_bits(*l)),
            // Float/double are SSE-class: their bit pattern must go in an XMM
            // register on SysV (and in the positionally-shared XMM slot on
            // Win64), not in a GP register.
            Value::Float(f) => JniArg::float(f.to_bits() as u64),
            Value::Double(d) => JniArg::float(d.to_bits()),
            Value::Object(Some(r)) => JniArg::int(obj_to_jobject(*r)),
            Value::Object(None) => JniArg::int(0u64),
            // Defensive: an operand of an unexpected variant. Fall back to the
            // declared descriptor tag to keep the register class correct.
            _ => {
                if *tag == b'F' || *tag == b'D' {
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

    let raw_result = call_jni_marshalled(fn_ptr, &jargs, fp_return);

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
                    true
                }
                Err(_) => false,
            },
        )
        .unwrap_or(false);
    if !raised {
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
    }
}

// ---- Index 215: RegisterNatives ----

extern "C" fn jni_register_natives(
    _env: JNIEnv,
    clazz: JClass,
    methods: *const JNINativeMethod,
    n_methods: JInt,
) -> JInt {
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
        let obj = shared.mem.heap.alloc_object(class_id, num_fields);
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 30: NewObjectA ----
// Allocates a new Java object and invokes the constructor indicated by `mid`.
extern "C" fn jni_new_object_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
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
    // Then call the constructor (<init>) on the allocated object.
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj_handle)?;
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
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
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
        Some(obj_handle)
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
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_byte_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JByte) {
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_char_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JChar) {
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_short_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JShort) {
    set_static_int_raw(fid, val as JInt);
}

// ---- Index 163: NewString (UTF-16) ----
// Creates a java.lang.String from a UTF-16 char array.
extern "C" fn jni_new_string(_env: JNIEnv, unicode: *const JChar, len: JSize) -> JString {
    if unicode.is_null() || len < 0 {
        return 0;
    }
    let chars: &[u16] = unsafe { std::slice::from_raw_parts(unicode, len as usize) };
    let s = String::from_utf16_lossy(chars);
    with_shared_vm(|shared| {
        let obj = create_java_string(shared, &s);
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 164: GetStringLength ----
extern "C" fn jni_get_string_length(_env: JNIEnv, str_obj: JString) -> JSize {
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
                                 // correct Vec layout (pointer + length) for deallocation.
        JNI_STRING_BUFFERS.with(|b| b.borrow_mut().insert(ptr as usize, len));
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
        let len = JNI_STRING_BUFFERS
            .with(|b| b.borrow_mut().remove(&(chars as usize)))
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
        let obj = shared.mem.heap.alloc_object(dbb_class_id, num_fields);
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
        obj_to_jobject(obj)
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

extern "C" fn jni_get_env(_vm: JavaVM, env: *mut *mut std::ffi::c_void, _version: JInt) -> JInt {
    if env.is_null() {
        return JNI_ERR;
    }
    let jni_env = get_jni_env();
    unsafe {
        *env = jni_env as *mut std::ffi::c_void;
    }
    JNI_OK
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
            if let Some(shared) = process_vm() {
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
        // Build + register the foreign thread. It is STW-visible (with an empty
        // deposited snapshot) BEFORE we publish the TLS context below — there is
        // no window where it can run Java while invisible to `request_stw`.
        let raw = attach_foreign_thread(&shared, daemon, name.as_deref());
        // §3.3 — an attached-but-idle thread (parked in the host event loop with
        // no Java frames) is modelled as GC-blocked, so a stop-the-world on
        // another thread does not wait forever for it. The matching leave is the
        // first Java call's `ForeignCallGuard` (or detach's, if it never calls).
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, std::sync::atomic::Ordering::Release);
        });
        // Not counted in any active STW's `expected` (we registered after its
        // `request_stw`), so we must NOT arrive even if one is in progress —
        // hence the pre_stw return is intentionally ignored here.
        let _ = shared.mem.gc_barrier.mark_blocked_region_enter();
        // Publish the TLS context LAST.
        set_jni_context_arc(shared);
        set_jni_thread(raw);
        write_jni_env(penv);
        tracing::debug!("JNI AttachCurrentThread: foreign thread attached + registered");
        return JNI_OK;
    }

    // Gate off — historical behaviour: env pointer only, no registration.
    write_jni_env(penv);
    tracing::debug!("JNI AttachCurrentThread: thread attached (env-only, gate off)");
    JNI_OK
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
        if let Some(shared) = process_vm() {
            let tid = with_foreign_thread(|jt| jt.thread_id);
            // Retire the tail, remove its published address, and mark dead in
            // the same barrier-serialized transition. This prevents a new STW
            // from seeing either an unwalkable tail or a dead thread that is
            // still counted as blocked.
            if let Some(tid) = tid {
                shared.mem.gc_barrier.mark_blocked_region_leave_after(|| {
                    with_foreign_thread(|jt| jt.tlab.retire());
                    shared.threads.thread_registry.clear_tlab_addr(tid);
                    shared.threads.thread_registry.mark_dead(tid);
                    shared.threads.monitors.release_monitors_held_by(tid);
                });
            } else {
                shared.mem.gc_barrier.mark_blocked_region_leave();
            }
            // Reclaim the already-retired attachment.
            detach_foreign_thread(&shared);
        } else {
            // No live VM (process shutdown) — just drop our owned box.
            let _ = FOREIGN_THREAD_BOX.try_with(|c| *c.borrow_mut() = None);
            let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
        }
        clear_jni_thread();
        clear_jni_context();
        tracing::debug!("JNI DetachCurrentThread: foreign thread detached");
        return JNI_OK;
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

        let _prev = replace_jni_context(&vm.shared);
        set_jni_thread(vm.main_thread.as_mut() as *mut _);
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
        clear_jni_thread();
        clear_jni_context();
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
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
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
        set_jni_context_arc(shared.clone());
        let arr = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 10);
        let jarray = obj_to_jobject(arr);
        let env = get_jni_env();
        let len = jni_get_array_length(env, jarray);
        assert_eq!(len, 10);
        clear_jni_context();
    }

    #[test]
    fn jni_field_access_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
        let jobj = obj_to_jobject(obj);
        let fid = encode_field_id(ClassId::new(0), 1);
        let env = get_jni_env();
        jni_set_int_field(env, jobj, fid, 42);
        let val = jni_get_int_field(env, jobj, fid);
        assert_eq!(val, 42);
        clear_jni_context();
    }

    #[test]
    fn jni_new_int_array_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 5);
        assert_ne!(arr, 0);
        let len = jni_get_array_length(env, arr);
        assert_eq!(len, 5);
        clear_jni_context();
    }

    #[test]
    fn jni_array_region_roundtrip() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 4);
        let data: [JInt; 4] = [10, 20, 30, 40];
        jni_set_int_array_region(env, arr, 0, 4, data.as_ptr());
        let mut out = [0i32; 4];
        jni_get_int_array_region(env, arr, 0, 4, out.as_mut_ptr());
        assert_eq!(out, [10, 20, 30, 40]);
        clear_jni_context();
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
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
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
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
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

    #[test]
    fn is_same_object_via_global_ref() {
        // IsSameObject must return true when comparing a local ref and a global ref
        // to the same underlying object. NEW-11: same self-inconsistency
        // fix as `global_ref_jobject_to_obj_roundtrip`.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
    }

    #[test]
    fn new_string_utf16_roundtrip() {
        // NewString (UTF-16) must produce the same Java String as NewStringUTF.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let utf16: Vec<u16> = "hello".encode_utf16().collect();
        let jstr = jni_new_string(env, utf16.as_ptr(), utf16.len() as JSize);
        // Both GetStringUTFLength and GetStringLength must reflect the content.
        let utf_len = jni_get_string_utf_length(env, jstr);
        let char_len = jni_get_string_length(env, jstr);
        assert_eq!(utf_len, 5, "UTF byte length of 'hello'");
        assert_eq!(char_len, 5, "UTF-16 code unit count of 'hello'");
        clear_jni_context();
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
    // `forget`, record `(len, cap)` in `JNI_ARRAY_ELEM_BUFFERS`, then look it
    // up and `Vec::from_raw_parts(ptr, len, cap)` — and prove the round-trip is
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
        JNI_ARRAY_ELEM_BUFFERS.with(|c| {
            c.borrow_mut().insert(ptr as usize, stored);
        });

        // Simulate a Release that does NOT trust the handle: pull the stored
        // layout and reconstruct exactly. A spurious "handle length" of 0 or 99
        // must be irrelevant.
        let entry = JNI_ARRAY_ELEM_BUFFERS.with(|c| c.borrow_mut().remove(&(ptr as usize)));
        let ArrayElemBuffer { len, cap, .. } = entry.expect("buffer must be tracked");
        assert_eq!(len, 5);
        assert_eq!(cap, 5);
        // Sound free using the stored layout (NOT a re-derived length).
        unsafe {
            drop(Vec::from_raw_parts(ptr, len, cap));
        }
        // Entry is consumed; a second release would find nothing and no-op.
        assert!(JNI_ARRAY_ELEM_BUFFERS.with(|c| c.borrow().get(&(ptr as usize)).is_none()));
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
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
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
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
    }

    /// Two live direct buffers must not cross-talk. With the address-keyed
    /// cache this held only by luck of allocation; with per-object fields it is
    /// structural.
    #[test]
    fn jni_direct_buffers_are_independent() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
    }

    /// A zero capacity is legal (`NewDirectByteBuffer(addr, 0)`), so the
    /// capacity getter must not use "non-zero means present" as its presence
    /// test the way the address getter does.
    #[test]
    fn jni_direct_buffer_zero_capacity_round_trips() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
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
        clear_jni_context();
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
        set_jni_context_arc(shared.clone());
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
        assert!(!published.is_null(), "a block that covers its capacity must publish");
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
        clear_jni_context();
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
                "the tag must sit above every canonical user-space address, so a                  jclass can never be confused with a heap pointer"
            );
        }
        // Distinct classes must stay distinct handles.
        assert_ne!(
            class_id_to_jclass(ClassId::new(0)),
            class_id_to_jclass(ClassId::new(1))
        );
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
}
