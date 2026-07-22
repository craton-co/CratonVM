// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JNI (Java Native Interface) implementation.
//!
//! Provides the standard C API for native code to interact with the JVM.
//! The function table is represented as a flat `[usize; 229]` array matching
//! the JNI spec layout. Native code accesses it via `(*env)[index](env, ...)`.
//!
//! JNI functions access the VM via thread-local storage. Before calling into
//! native code, the interpreter sets the TLS context with `set_jni_context()`,
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
pub const JNI_FUNCTION_COUNT: usize = 234;
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
    /// so we record `buffer_ptr -> object_base` here and look it up to UNPIN.
    /// Refcounted in `pinned`, so overlapping checkouts of the same array are safe.
    static JNI_CRITICAL_PINS: std::cell::RefCell<HashMap<usize, usize>> =
        std::cell::RefCell::new(HashMap::new());
    /// Thread-local cache for parsed method descriptors.
    /// Maps descriptor string → parsed parameter type tags, avoiding
    /// repeated parsing of the same descriptor in hot JNI call paths.
    static JNI_DESCRIPTOR_CACHE: std::cell::RefCell<DescriptorCache> =
        std::cell::RefCell::new(DescriptorCache::new());
}

/// Set the JNI thread-local context before entering native code.
///
/// Stores an `Arc<SharedVm>` in thread-local storage, keeping the VM alive
/// via reference counting for the entire duration of the native call.
/// This eliminates the use-after-free risk of the previous raw-pointer design.
///
/// `clear_jni_context()` MUST be called after every native call returns
/// (including on panic/unwind paths) to release the `Arc` and allow the VM
/// to be dropped when no longer needed.
pub fn set_jni_context(shared: &SharedVm) {
    // Obtain an Arc from the SharedVm's weak self-reference.
    // This keeps the VM alive via ref-counting for the native call duration.
    let arc = shared.get_arc();
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = Some(arc);
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Set JNI context directly from an existing `Arc<SharedVm>`.
/// Preferred when the caller already holds an Arc (avoids weak-reference upgrade).
pub fn set_jni_context_arc(shared: Arc<SharedVm>) {
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = Some(shared);
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Clear the JNI thread-local context after returning from native code.
/// Drops the `Arc<SharedVm>`, decrementing the reference count.
pub fn clear_jni_context() {
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = None;
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

/// Clear the JNI thread pointer on native code return.
pub fn clear_jni_thread() {
    JNI_THREAD.with(|c| c.set(std::ptr::null_mut()));
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
    *PROCESS_VM.lock() = Some(Arc::downgrade(shared));
}

/// Resolve the live process-global VM, if one was published and is still alive.
/// Returns an owning `Arc` (keeps the VM alive for the duration of the caller's
/// use) or `None` if no VM was created or it has been dropped.
pub fn process_vm() -> Option<Arc<SharedVm>> {
    PROCESS_VM.lock().as_ref().and_then(Weak::upgrade)
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
}

/// True if the calling OS thread is currently foreign-attached (owns a
/// `JvmThread` parked in [`FOREIGN_THREAD_BOX`]).
pub fn is_foreign_attached() -> bool {
    FOREIGN_THREAD_BOX.with(|c| c.borrow().is_some())
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
    let tid = shared.thread_registry.next_thread_id();
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
        .thread_registry
        .register_with_daemon(tid, &name, None, daemon);
    shared
        .thread_registry
        .set_interrupted_flag(tid, jt.interrupted.clone());
    shared
        .thread_registry
        .set_park_state(tid, jt.park_state.clone());
    shared
        .thread_registry
        .set_root_snapshot(tid, jt.root_snapshot.clone());
    shared
        .thread_registry
        .set_frame_trace(tid, jt.frame_trace.clone());
    shared
        .thread_registry
        .set_gc_block_state(tid, jt.gc_block_state.clone());
    // BUG-03 — publish this foreign thread's TLAB address (the box is
    // address-stable) so the cross-thread STW JIT root scan can recover its
    // un-retired reserved tail if forcibly stopped mid-JIT. Cleared in
    // `detach_foreign_thread` before the box is dropped.
    shared
        .thread_registry
        .set_tlab_addr(tid, &jt.tlab as *const cratonvm_gc::Tlab as usize);
    // XT-FRAME-SCAN: publish this foreign thread's `JvmThread` address too
    // (the same Box keeps it stable) so a takeover that freezes it mid-JIT
    // can walk its interpreter frames. Cleared with the TLAB address in
    // `detach_foreign_thread`.
    shared
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
    let jt = FOREIGN_THREAD_BOX.with(|c| c.borrow_mut().take());
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
    shared.heap.flush_thread_satb();
    // Retire the TLAB: install its tail filler and reset, so the unfilled tail
    // is walkable BEFORE this thread stops publishing its tail.  Clearing the
    // registry entry or marking it dead first lets a later non-moving sweep
    // observe raw zeroed tail bytes with no owner from which to recover a skip
    // span, desynchronizing the linear walk.
    jt.tlab.retire();
    // The tail is now a real walker-visible filler, so it is safe to remove
    // the address before the boxed `JvmThread` is dropped.
    shared.thread_registry.clear_tlab_addr(tid);
    // Drop out of `alive_count` / STW `expected` before reclaiming the TLAB so a
    // subsequent `request_stw` no longer waits for this thread.
    shared.thread_registry.mark_dead(tid);
    // A thread torn down while blocked inside a native call made from within
    // a `synchronized` region never executes its `monitorexit` bytecode —
    // release anything it still holds so no future locker waits forever
    // (see `MonitorTable::release_monitors_held_by`).
    shared.monitors.release_monitors_held_by(tid);
    drop(jt);
    FOREIGN_CALL_DEPTH.with(|c| c.set(0));
    true
}

/// Run `f` against this OS thread's foreign-attached `JvmThread`, if any.
///
/// Safe to call only at points where the interpreter does NOT also hold the
/// `&mut JvmThread` (which it borrows from the raw `JNI_THREAD` pointer): namely
/// the attach/detach paths and the foreign-call transitions, which run strictly
/// before a call begins or after it returns — never concurrently with the call.
fn with_foreign_thread<R>(f: impl FnOnce(&mut JvmThread) -> R) -> Option<R> {
    FOREIGN_THREAD_BOX.with(|c| c.borrow_mut().as_mut().map(|b| f(&mut **b)))
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
pub fn host_thread_enter_native() -> bool {
    if is_foreign_attached() {
        // Already auto-managed as idle-blocked between calls; nothing to do.
        return true;
    }
    let shared = match process_vm() {
        Some(s) => s,
        None => return false,
    };
    let pre_stw = shared.gc_barrier.mark_blocked_region_enter();
    if pre_stw {
        // A stop-the-world was already active when we incremented the blocked
        // count, so `request_stw` had counted this thread in `expected` (it was
        // a live, non-blocked mutator at that instant). Arrive exactly once so
        // the initiator's `wait_for_all` can complete. The id only selects "am I
        // the initiator" — a thread declaring itself in-native is never the
        // initiator — and a non-foreign caller here is the creating thread (id 0).
        // GCAUDIT-0711-FIX (finding 1a): auto for uniformity - this call
        // site never raises in_blocked_region, so it resolves identically.
        let _ = shared.gc_barrier.arrive_and_wait_auto(ThreadId(0));
    }
    true
}

/// Re-enter the VM after [`host_thread_enter_native`]: the current thread rejoins
/// the mutator population (it will wait out any in-flight stop-the-world first).
/// Must balance exactly one prior `host_thread_enter_native`. No-op for a foreign
/// attached thread (auto-managed) or when no VM exists.
pub fn host_thread_leave_native() -> bool {
    if is_foreign_attached() {
        return true;
    }
    let shared = match process_vm() {
        Some(s) => s,
        None => return false,
    };
    shared.gc_barrier.mark_blocked_region_leave();
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
/// docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md section 5.8 for the
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
/// docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md section 5.8). Sized
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
    let _ = shared.gc_barrier.mark_blocked_region_enter();
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
            shared.gc_barrier.mark_blocked_region_leave_after(|| {
                // Keep the retiring tail and the liveness transition in the
                // barrier-serialized closure. A new STW must not observe this
                // thread as dead before the tail has become walkable.
                with_foreign_thread(|jt| jt.tlab.retire());
                shared.thread_registry.clear_tlab_addr(tid);
                shared.thread_registry.mark_dead(tid);
                shared.monitors.release_monitors_held_by(tid);
            });
        } else {
            shared.gc_barrier.mark_blocked_region_leave();
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
        std::env::var("CRATONVM_FOREIGN_ATTACH").as_deref(),
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
        // class (Result 3, docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md
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
        shared.gc_barrier.mark_blocked_region_leave();
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
            // docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md's 5.8
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
            let pre_stw = shared.gc_barrier.mark_blocked_region_enter();
            if pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): the in_blocked_region store
                // above already ran, so this pause may already have
                // excluded us - auto resolves it from that pause's own
                // exclusion snapshot instead of assuming participation.
                let tid = with_foreign_thread(|jt| jt.thread_id).unwrap_or(ThreadId(0));
                let _ = shared.gc_barrier.arrive_and_wait_auto(tid);
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
                    tracing::warn!("JNI context generation changed during callback ({} -> {}) — possible nested native call", gen_before, gen_after);
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
        _ => None,
    }
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
        let obj_class_id = shared.heap.class_id_of(oref);
        let (decl_class_id, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
        invoke_on_class_shared(
            shared,
            thread,
            obj_class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        )
        .ok()
        .flatten()
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
            ClassId::new(clazz as u32)
        } else {
            let (dcid, _) = decode_method_id(mid);
            dcid
        };
        let (_, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(dispatch_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
        invoke_on_class_shared(
            shared,
            thread,
            dispatch_class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        )
        .ok()
        .flatten()
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
            ClassId::new(clazz as u32)
        } else {
            decl_class_id
        };
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let jvm_args = unsafe { jvalues_to_values(args, &param_types) };
        invoke_on_class_shared(
            shared,
            thread,
            class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        )
        .ok()
        .flatten()
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
    pub fn update_after_gc(&mut self, pointer_map: &std::collections::HashMap<usize, usize>) {
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
pub fn update_local_refs_after_gc(pointer_map: &std::collections::HashMap<usize, usize>) {
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
/// the matching `Release` passes back), while the pin is keyed on the OBJECT
/// BASE (`oref.as_ptr()`), the address a collector tests against the pin set.
fn pin_critical_array(oref: ObjectRef, data_ptr: usize) {
    let base = oref.as_ptr() as usize;
    if base == 0 || data_ptr == 0 {
        return;
    }
    cratonvm_gc::pinned::pin(base);
    JNI_CRITICAL_PINS.with(|c| {
        c.borrow_mut().insert(data_ptr, base);
    });
}

/// Undo a [`pin_critical_array`] for the buffer at `data_ptr`. Looks up the
/// pinned object base recorded at Get time and unpins it (refcounted, so an
/// overlapping Get on the same array keeps it pinned until its own Release).
/// A `data_ptr` we never handed out (foreign pointer / double-release) is
/// absent from the map and ignored.
fn unpin_critical_array(data_ptr: usize) {
    if data_ptr == 0 {
        return;
    }
    let base = JNI_CRITICAL_PINS.with(|c| c.borrow_mut().remove(&data_ptr));
    if let Some(base) = base {
        cratonvm_gc::pinned::unpin(base);
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
                Some(shared) => shared.jni_global_refs.lock().resolve(jobj),
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
                Some(shared) => shared.heap.is_heap_addr(jobj as usize),
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
        Some(class_id.as_u32() as JClass)
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
        let class_id = ClassId::new(clazz as u32);
        let cm = shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        class.superclass.map(|sc| sc.as_u32() as JClass)
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
        let sub_id = ClassId::new(sub as u32);
        let sup_id = ClassId::new(sup as u32);
        if sub_id == sup_id {
            return JNI_TRUE;
        }
        let cm = shared.class_manager.read();
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
        let class_id = ClassId::new(clazz as u32);
        let class_name = {
            let cm = shared.class_manager.read();
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
extern "C" fn jni_exception_describe(_env: JNIEnv) {}

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

// ---- Index 22: NewGlobalRef ----
extern "C" fn jni_new_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    if obj == 0 {
        return 0;
    }
    // Resolve the object whether it's a local ref or another global ref.
    let oref = match jobject_to_obj(obj) {
        Some(r) => r,
        None => return 0,
    };
    with_shared_vm(|shared| {
        let handle = shared.jni_global_refs.lock().add(oref);
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
    with_shared_vm(|shared| {
        shared.jni_global_refs.lock().remove(gref);
    });
}

// ---- Index 24: DeleteLocalRef ----
extern "C" fn jni_delete_local_ref(_env: JNIEnv, lref: JObject) {
    delete_local_ref(lref);
}

// ---- Index 25: IsSameObject ----
extern "C" fn jni_is_same_object(_env: JNIEnv, a: JObject, b: JObject) -> JBoolean {
    // Resolve both sides through the global-ref layer so that comparing a
    // local ref and a global ref to the same object returns true.
    match (jobject_to_obj(a), jobject_to_obj(b)) {
        (None, None) => JNI_TRUE,
        (Some(ra), Some(rb)) if ra == rb => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

// ---- Index 26: NewLocalRef ----
extern "C" fn jni_new_local_ref(_env: JNIEnv, obj: JObject) -> JObject {
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
        let class_id = shared.heap.class_id_of(oref);
        Some(class_id.as_u32() as JClass)
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
        let obj_class_id = shared.heap.class_id_of(oref);
        let target_id = ClassId::new(clazz as u32);
        if obj_class_id == target_id {
            return Some(JNI_TRUE);
        }
        // Walk superclass chain
        let cm = shared.class_manager.read();
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
        let class_id = ClassId::new(clazz as u32);
        let resolved = {
            let cm = shared.class_manager.read();
            shared
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
        let class_id = ClassId::new(clazz as u32);
        let resolved = {
            let cm = shared.class_manager.read();
            shared
                .link_resolver
                .resolve_or_compute(class_id, name_str, sig_str, || {
                    let result = find_field_recursive(class_id, name_str, &cm.class_store);
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
        match shared.heap.get_field(oref, field_index) {
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
        match shared.heap.get_field(oref, field_index) {
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
        match shared.heap.get_field(oref, field_index) {
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
        match shared.heap.get_field(oref, field_index) {
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
        shared.heap.set_field(oref, field_index, value);
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
        if bits != 0 && bits & 0x7 == 0 && shared.heap.is_object_address(bits as usize).is_some() {
            crate::memory::smuggled_longs::record_minted_long(bits);
        }
        shared.heap.set_field(oref, field_index, Value::Long(val));
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
        shared.heap.set_field(oref, field_index, Value::Float(val));
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
        shared.heap.set_field(oref, field_index, Value::Double(val));
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
        let statics = shared.statics.read();
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
        let statics = shared.statics.read();
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
        let statics = shared.statics.read();
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
        let statics = shared.statics.read();
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
        let mut statics = shared.statics.write();
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
        let mut statics = shared.statics.write();
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
        let mut statics = shared.statics.write();
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
        let mut statics = shared.statics.write();
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
        let s = read_java_string(&shared.heap, oref)?;
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
        let s = read_java_string(&shared.heap, oref)?;
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
        Some(shared.heap.array_length(oref) as JSize)
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
        let component_id = ClassId::new(clazz as u32);
        let arr =
            shared
                .heap
                .alloc_array(component_id, ArrayElementType::Reference, length as usize);
        // Initialize elements if init is non-null
        if init != 0 {
            if let Some(init_ref) = jobject_to_obj(init) {
                for i in 0..length as usize {
                    let _ = shared
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
        match shared.heap.get_array_element(oref, index as usize) {
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
        let _ = shared.heap.set_array_element(oref, index as usize, value);
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
                let len = shared.heap.array_length(oref);
                // `len.max(1)` guarantees a real, uniquely-addressed allocation
                // even for a zero-length array, so its buffer pointer is never a
                // shared dangling sentinel that would collide in the maps below.
                // Release uses the STORED capacity (not `len`), so over-allocating
                // by one element for the empty case stays sound.
                let mut buf: Vec<$rust_type> = Vec::with_capacity(len.max(1));
                for i in 0..len {
                    let val = match shared.heap.get_array_element(oref, i) {
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
                let array_gref = shared.jni_global_refs.lock().add(oref);
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
                    let oref = shared.jni_global_refs.lock().resolve(array_gref)?;
                    let arr_len = shared.heap.array_length(oref);
                    let copy_len = stored_len.min(arr_len);
                    for i in 0..copy_len {
                        let val = unsafe { *elems.add(i) };
                        let value = $value_constructor(val);
                        let _ = shared.heap.set_array_element(oref, i, value);
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
                    shared.jni_global_refs.lock().remove(array_gref);
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
                if !region_bounds_ok(start, len, shared.heap.array_length(oref)) {
                    return None;
                }
                for i in 0..len as usize {
                    let val = shared
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
                if !region_bounds_ok(start, len, shared.heap.array_length(oref)) {
                    return None;
                }
                for i in 0..len as usize {
                    let val = unsafe { *buf.add(i) };
                    let _ = shared.heap.set_array_element(
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
        if !region_bounds_ok(start, len, shared.heap.array_length(oref)) {
            return None;
        }
        for i in 0..len as usize {
            let val = unsafe { *buf.add(i) };
            let bits = val as u64;
            if bits != 0
                && bits & 0x7 == 0
                && shared.heap.is_object_address(bits as usize).is_some()
            {
                crate::memory::smuggled_longs::record_minted_long(bits);
            }
            let _ = shared
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
        if let Some(m) = shared.monitors.enter_or_contend(oref, ThreadId(0)) {
            let blk = shared.gc_barrier.enter_blocked();
            if blk.pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): auto for uniformity.
                let _ = shared.gc_barrier.arrive_and_wait_auto(ThreadId(0));
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
        let _ = shared.monitors.exit(oref, ThreadId(0));
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
    JNI_FALSE
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
        let mut cm = shared.class_manager.write();
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
            let _ = shared.jit_cache.write().invalidate_for_class(n);
            let _ = shared.invalidate_jit_for_class(n);
        }
        Some(cid.as_u32() as JClass)
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
        let class_id_val = match shared.heap.get_field(oref, 0) {
            Value::Int(i) => i as u32,
            _ => return None,
        };
        let method_idx = match shared.heap.get_field(oref, 1) {
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
        let class_id_val = match shared.heap.get_field(oref, 0) {
            Value::Int(i) => i as u32,
            _ => return None,
        };
        let field_idx = match shared.heap.get_field(oref, 1) {
            Value::Int(i) => i as usize,
            _ => return None,
        };
        Some(encode_field_id(ClassId::new(class_id_val), field_idx))
    })
    .flatten()
    .unwrap_or(0)
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
            ClassId::new(clazz as u32)
        } else {
            decl_class_id
        };
        // Allocate a synthetic Method object with class_id and method_index in fields.
        // Resolve the real `java.lang.reflect.Method` class — `find_class_by_name`
        // only sees already-loaded classes, so `load_class_concurrent` is used to
        // force the load. Allocating with `ClassId::new(0)` (`java/lang/Object`,
        // zero declared fields) but 4 slots produces an undersized object the
        // GC's `get_field` bounds guard rejects.
        let method_class_id = shared
            .load_class_concurrent("java/lang/reflect/Method")
            .unwrap_or_else(|_| {
                shared
                    .class_manager
                    .write()
                    .ensure_synthetic_class("java/lang/reflect/Method", 4)
            });
        let num_fields = shared
            .class_manager
            .read()
            .get_class(method_class_id)
            .map_or(4, |c| c.num_total_fields.max(4));
        let obj = shared.heap.alloc_object(method_class_id, num_fields);
        shared
            .heap
            .set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared
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
            ClassId::new(clazz as u32)
        } else {
            decl_class_id
        };
        // Resolve the real `java.lang.reflect.Field` class (see the matching
        // comment in `jni_to_reflected_method`): allocating with
        // `ClassId::new(0)` + 4 slots produces an undersized object the GC's
        // `get_field` bounds guard rejects.
        let field_class_id = shared
            .load_class_concurrent("java/lang/reflect/Field")
            .unwrap_or_else(|_| {
                shared
                    .class_manager
                    .write()
                    .ensure_synthetic_class("java/lang/reflect/Field", 4)
            });
        let num_fields = shared
            .class_manager
            .read()
            .get_class(field_class_id)
            .map_or(4, |c| c.num_total_fields.max(4));
        let obj = shared.heap.alloc_object(field_class_id, num_fields);
        shared
            .heap
            .set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared
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
        let s = read_java_string(&shared.heap, oref)?;
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
        let s = read_java_string(&shared.heap, oref)?;
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
        let bytes = region.as_bytes();
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
        let element_type = shared.heap.array_element_type(oref)?;
        let stride = critical_stride(element_type);
        if stride == 0 {
            return None; // reference array — not a primitive critical
        }
        let len = shared.heap.array_length(oref);
        let mut buf: Vec<u8> = vec![0u8; len * stride];
        for i in 0..len {
            let v = match shared.heap.get_array_element(oref, i) {
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
        let pinned_regions = shared.heap.pin_critical_region(oref);
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
                let _ = shared.heap.set_array_element(oref, i, v);
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
                shared.heap.unpin_critical_regions(&copy.pinned_regions);
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
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let mut refs = shared.jni_global_refs.lock();
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
    with_shared_vm(|shared| {
        let mut refs = shared.jni_global_refs.lock();
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
// Internal helpers for field access
// ---------------------------------------------------------------------------

fn get_int_field_raw(obj: JObject, field_id: JFieldID) -> JInt {
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.heap.get_field(oref, field_index) {
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
        shared.heap.set_field(oref, field_index, Value::Int(val));
        Some(())
    });
}

fn get_static_int_raw(clazz: JClass, field_id: JFieldID) -> JInt {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.statics.read();
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
        let mut statics = shared.statics.write();
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

/// Global table of JNI native function pointers registered via `RegisterNatives`.
/// Key = FNV-1a hash of "class_name.method_nameDescriptor".
/// Value = raw function pointer (to be called with `dispatch_jni_native`).
static JNI_NATIVE_METHODS: std::sync::LazyLock<parking_lot::RwLock<HashMap<u64, usize>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(HashMap::new()));

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

/// Store a JNI function pointer registered via `RegisterNatives` or symbol lookup.
pub fn register_jni_native(class_name: &str, method_name: &str, descriptor: &str, fn_ptr: usize) {
    let key = jni_native_key(class_name, method_name, descriptor);
    let mut table = JNI_NATIVE_METHODS.write();
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

/// Look up a JNI function pointer for the given method.
/// Returns `None` if no pointer was registered.
pub fn find_jni_native(class_name: &str, method_name: &str, descriptor: &str) -> Option<usize> {
    let key = jni_native_key(class_name, method_name, descriptor);
    JNI_NATIVE_METHODS.read().get(&key).copied()
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
            c if c.is_ascii() => out.push(c),
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
    native_libraries: &parking_lot::Mutex<Vec<libloading::Library>>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let short = jni_short_name(class_name, method_name);
    let long = jni_long_name(class_name, method_name, descriptor);

    let libs = native_libraries.lock();
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
                    register_jni_native(class_name, method_name, descriptor, fn_ptr);
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

/// Dispatch a JNI native function pointer call.
///
/// Converts `Value` args to 64-bit C values (correct for all integer/reference
/// types on x86-64). Float/double args are passed as their bit representation
/// in integer registers — this is ABI-correct on Windows x64, but not on
/// Linux x86-64 System V for the float/double parameter positions.
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
            Value::Long(l) => JniArg::int(*l as u64),
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
        b'Z' => Value::Int((raw_result & 1) as i32),
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
                    if shared.heap.is_object_address(bits as usize).is_some() {
                        crate::memory::smuggled_longs::record_minted_long(bits);
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
/// Returns `false` (and logs) for null / misaligned pointers.
#[inline]
fn jni_fn_ptr_ok(fn_ptr: usize) -> bool {
    if fn_ptr == 0 {
        tracing::error!("JNI call with null function pointer — returning 0");
        return false;
    }
    // Alignment check: function pointers must be at least 2-byte aligned
    // on all modern architectures (4-byte on ARM).
    #[cfg(target_arch = "aarch64")]
    if fn_ptr % 4 != 0 {
        tracing::error!(
            "JNI call with misaligned function pointer {:#x} — returning 0",
            fn_ptr
        );
        return false;
    }
    #[cfg(not(target_arch = "aarch64"))]
    if fn_ptr % 2 != 0 {
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

    // Resolve the class name from the JClass mirror
    let class_name = with_shared_vm(|shared| {
        let oref = jobject_to_obj(clazz)?;
        let class_id = shared.heap.class_id_of(oref);
        shared
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
        register_jni_native(&class_name, &name, &sig, fn_ptr);
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
    let class_name = with_shared_vm(|shared| {
        let oref = jobject_to_obj(clazz)?;
        let class_id = shared.heap.class_id_of(oref);
        shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
    })
    .flatten();

    if let Some(class_name) = class_name {
        // Get all methods for this class and remove their native registrations
        let methods_to_remove: Vec<u64> = with_shared_vm(|shared| {
            let cm = shared.class_manager.read();
            let mut keys = Vec::new();
            // Find the class and iterate its methods
            if let Some(class_id) = cm.find_class_by_name(&class_name) {
                if let Some(class) = cm.get_class(class_id) {
                    for method in &class.methods {
                        if method.is_native() {
                            let key = jni_native_key(&class_name, &method.name, &method.descriptor);
                            keys.push(key);
                        }
                    }
                }
            }
            keys
        })
        .unwrap_or_default();

        if !methods_to_remove.is_empty() {
            let mut table = JNI_NATIVE_METHODS.write();
            for key in &methods_to_remove {
                table.remove(key);
            }
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
        let class_id = ClassId::new(clazz as u32);
        // `clazz` must resolve to a real class. If it does not (stale or
        // bogus handle), return null rather than allocating an object against
        // an unresolved/`ClassId(0)` class: a wrongly-classed object whose
        // declared field count is unknown corrupts every later field access.
        let num_fields = match shared
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
        let obj = shared.heap.alloc_object(class_id, num_fields);
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
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
        let _ = invoke_on_class_shared(
            shared,
            thread,
            ClassId::new(clazz as u32),
            &method_name,
            &descriptor,
            &jvm_args,
        );
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
        let s = read_java_string(&shared.heap, oref)?;
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
        let s = read_java_string(&shared.heap, oref)?;
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

/// Read a JNI method's arguments from a va_list and pack them into a JValue
/// array.  The caller is responsible for passing the correct `mid` so that we
/// can look up the method descriptor and determine argument types.
///
/// Returns `std::ptr::null()` on failure; otherwise returns a heap-allocated
/// JValue slice that the caller must free with `Box::from_raw`.
fn va_list_to_jvalues(mid: JMethodID, mut va: VaList) -> (*const JValue, usize) {
    if mid == 0 || va.is_null() {
        return (std::ptr::null(), 0);
    }
    // Look up the method descriptor to know argument types.
    let descriptor = with_shared_vm(|shared| {
        let (decl_class_id, method_index) = decode_method_id(mid);
        let cm = shared.class_manager.read();
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
    for &tag in &param_types {
        // On all supported platforms, va_list args are 8-byte slots.
        let raw: u64 = unsafe {
            let val = *(va as *const u64);
            va = va.add(8);
            val
        };
        let jv = match tag {
            b'Z' => JValue { z: raw as JBoolean },
            b'B' => JValue { b: raw as JByte },
            b'C' => JValue { c: raw as JChar },
            b'S' => JValue { s: raw as JShort },
            b'I' => JValue { i: raw as JInt },
            b'J' => JValue { j: raw as JLong },
            b'F' => JValue { f: f32::from_bits(raw as u32) },
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
        drop(Box::from_raw(std::slice::from_raw_parts_mut(
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
// DirectByteBuffer objects are represented as regular Java objects with two
// special fields: a native memory address (long) and a capacity (int).
// We store these in fields [0] (address as long) and [1] (capacity as long).

/// Wrapper around a raw pointer to make it Send+Sync for the global registry.
/// Safety: direct buffer memory is allocated via malloc and is valid for the lifetime of the buffer.
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

/// Global registry of direct buffer metadata: maps JObject handle → (address, capacity).
static DIRECT_BUFFERS: std::sync::LazyLock<parking_lot::Mutex<HashMap<u64, (SendPtr, i64)>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

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
        // Allocate a java/nio/DirectByteBuffer-like object with 2 fields
        // (address, capacity). The object's class must declare those 2
        // fields — allocating with `ClassId::new(0)` (`java/lang/Object`,
        // zero declared fields) yields an undersized object that the GC's
        // `get_field` bounds guard rejects on every access.
        let dbb_class_id = shared
            .load_class_concurrent("java/nio/DirectByteBuffer")
            .unwrap_or_else(|_| {
                shared
                    .class_manager
                    .write()
                    .ensure_synthetic_class("java/nio/DirectByteBuffer", 2)
            });
        let num_fields = shared
            .class_manager
            .read()
            .get_class(dbb_class_id)
            .map_or(2, |c| c.num_total_fields.max(2));
        let obj = shared.heap.alloc_object(dbb_class_id, num_fields);
        let handle = obj_to_jobject(obj);
        // Store the address as a long in field 0.
        shared.heap.set_field(obj, 0, Value::Long(address as i64));
        // Store the capacity in field 1.
        shared.heap.set_field(obj, 1, Value::Long(capacity));
        // Also register in our side-table for GetDirectBufferAddress.
        DIRECT_BUFFERS
            .lock()
            .insert(handle, (SendPtr(address), capacity));
        handle
    })
    .unwrap_or(0)
}

// Index 230: GetDirectBufferAddress
extern "C" fn jni_get_direct_buffer_address(_env: JNIEnv, buf: JObject) -> *mut u8 {
    if buf == 0 {
        return std::ptr::null_mut();
    }
    // First try the side-table (fast path for buffers we created).
    if let Some(&(SendPtr(addr), _)) = DIRECT_BUFFERS.lock().get(&buf) {
        return addr;
    }
    // Fall back to reading the address field from the object.
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        match shared.heap.get_field(oref, 0) {
            Value::Long(addr) => Some(addr as *mut u8),
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// Index 231: GetDirectBufferCapacity
extern "C" fn jni_get_direct_buffer_capacity(_env: JNIEnv, buf: JObject) -> JLong {
    if buf == 0 {
        return -1;
    }
    // First try the side-table.
    if let Some(&(_, cap)) = DIRECT_BUFFERS.lock().get(&buf) {
        return cap;
    }
    // Fall back to reading the capacity field.
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        match shared.heap.get_field(oref, 1) {
            Value::Long(cap) => Some(cap),
            _ => None,
        }
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

    // GetObjectRefType
    t[19] = jni_get_object_ref_type as *const () as usize;

    // Local/global frame management
    t[20] = jni_push_local_frame as *const () as usize;
    t[21] = jni_pop_local_frame as *const () as usize;

    // References
    t[22] = jni_new_global_ref as *const () as usize;
    t[23] = jni_delete_global_ref as *const () as usize;
    t[24] = jni_delete_local_ref as *const () as usize;
    t[25] = jni_is_same_object as *const () as usize;
    t[26] = jni_new_local_ref as *const () as usize;
    t[27] = jni_ensure_local_capacity as *const () as usize;
    t[28] = jni_alloc_object as *const () as usize;
    // t[28] = NewObject (varargs) — not implementable in stable Rust extern "C"
    t[29] = jni_new_object_v as *const () as usize;
    t[30] = jni_new_object_a as *const () as usize;

    // Object operations
    t[31] = jni_get_object_class as *const () as usize;
    t[32] = jni_is_instance_of as *const () as usize;

    // Method IDs
    t[33] = jni_get_method_id as *const () as usize;

    // Call<Type>Method/V/A — virtual instance (groups of 3: varargs, va_list, array)
    // Bare-varargs `...` slots (34,37,40,...) can't be dispatched in stable
    // Rust; wire them to a stub that raises UnsatisfiedLinkError so a native
    // calling them fails loudly instead of getting a fabricated 0/null. The
    // V (va_list) and A (jvalue[]) forms below are fully implemented.
    for slot in [34, 37, 40, 43, 46, 49, 52, 55, 58, 61] {
        t[slot] = jni_varargs_unsupported as *const () as usize;
    }
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

    // CallNonvirtual<Type>Method/V/A (groups of 3)
    // Bare-varargs `...` slots (64,67,70,...) raise UnsatisfiedLinkError; V/A wired below.
    for slot in [64, 67, 70, 73, 76, 79, 82, 85, 88, 91] {
        t[slot] = jni_varargs_unsupported as *const () as usize;
    }
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

    // CallStatic<Type>Method/V/A (groups of 3)
    // Bare-varargs `...` slots (114,117,120,...) raise UnsatisfiedLinkError; V/A wired below.
    for slot in [114, 117, 120, 123, 126, 129, 132, 135, 138, 141] {
        t[slot] = jni_varargs_unsupported as *const () as usize;
    }
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

    // GetObjectRefType (JNI 1.6) — also at slot 19 for compatibility
    t[232] = jni_get_object_ref_type as *const () as usize;

    // GetModule (JNI 9+)
    t[233] = jni_get_module as *const () as usize;

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
        let _ = shared.gc_barrier.mark_blocked_region_enter();
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
    // Foreign-attached thread: full teardown (deregister + reclaim JvmThread).
    if is_foreign_attached() {
        // JNI forbids detaching a thread that still has Java frames on its stack;
        // for us that means a call is in flight on this OS thread (depth > 0).
        // Return JNI_ERR rather than corrupt state (matches HotSpot).
        if FOREIGN_CALL_DEPTH.with(|c| c.get()) != 0 {
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
                shared.gc_barrier.mark_blocked_region_leave_after(|| {
                    with_foreign_thread(|jt| jt.tlab.retire());
                    shared.thread_registry.clear_tlab_addr(tid);
                    shared.thread_registry.mark_dead(tid);
                    shared.monitors.release_monitors_held_by(tid);
                });
            } else {
                shared.gc_barrier.mark_blocked_region_leave();
            }
            // Reclaim the already-retired attachment.
            detach_foreign_thread(&shared);
        } else {
            // No live VM (process shutdown) — just drop our owned box.
            FOREIGN_THREAD_BOX.with(|c| *c.borrow_mut() = None);
            FOREIGN_CALL_DEPTH.with(|c| c.set(0));
        }
        clear_jni_thread();
        clear_jni_context();
        tracing::debug!("JNI DetachCurrentThread: foreign thread detached");
        return JNI_OK;
    }

    // Non-foreign thread (bootstrap/creating thread, or never-attached): clear
    // the JNI TLS context if present — historical behaviour, unchanged.
    let had_context = JNI_SHARED_VM.with(|c| c.borrow().is_some());
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
        set_process_vm(&shared);
        // The attach path can now resolve the live VM from "the JavaVM*".
        let resolved = process_vm().expect("process_vm should resolve after publish");
        assert!(
            Arc::ptr_eq(&shared, &resolved),
            "process_vm must return the same SharedVm that was published"
        );
        // Dropping every owning Arc lets the Weak cell go dangling: process_vm
        // then reports no live VM rather than a use-after-free.
        drop(resolved);
        drop(shared);
        assert!(
            process_vm().is_none(),
            "process_vm must return None once the VM has been dropped"
        );
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

    #[test]
    fn foreign_attach_factory_registers_shares_and_detaches() {
        use crate::classloading::ClassId;
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _guard = PROCESS_VM_TEST_LOCK.lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        // A bare SharedVm has no registered threads (Vm::new registers main).
        let baseline = shared.thread_registry.alive_count();

        let raw = attach_foreign_thread(&shared, false, None);
        assert!(!raw.is_null());
        assert!(is_foreign_attached());
        assert_eq!(
            shared.thread_registry.alive_count(),
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
        let obj = shared.heap.alloc_object(ClassId::new(0), 1);
        jt.root_snapshot.lock().push(obj);
        let collected = shared.thread_registry.collect_all_root_snapshots();
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
        let blocked = shared.thread_registry.dump_blocked_states();
        assert!(
            blocked.iter().any(|(t, blk, _)| *t == tid.0 && *blk),
            "registry must observe the foreign thread's blocked state via the shared Arc"
        );

        // Detach reclaims the thread and restores the baseline.
        assert!(detach_foreign_thread(&shared));
        assert!(!is_foreign_attached());
        assert_eq!(
            shared.thread_registry.alive_count(),
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
        let main_tid = shared.thread_registry.next_thread_id();
        shared.thread_registry.register(main_tid, "main", None);

        // Attach a foreign thread and put it in the idle blocked region exactly
        // as `attach_current_thread_impl` does.
        let _raw = attach_foreign_thread(&shared, false, None);
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, std::sync::atomic::Ordering::Release)
        });
        let _ = shared.gc_barrier.mark_blocked_region_enter();

        // Two alive threads (main + foreign), but the idle foreign thread is
        // excluded from `expected`, so the initiator waits for nobody.
        let alive = shared.thread_registry.alive_count() as u32;
        assert_eq!(alive, 2);
        assert!(shared.gc_barrier.request_stw(main_tid, alive));
        assert_eq!(
            shared.gc_barrier.pending_count(),
            0,
            "idle foreign thread must be excluded from the STW expected-set"
        );
        shared.gc_barrier.wait_for_all(); // returns immediately — no deadlock
        shared.gc_barrier.complete_gc(HashMap::new());

        // Teardown mirrors detach: mark dead, leave the region, reclaim.
        let tid = with_foreign_thread(|jt| jt.thread_id).unwrap();
        shared.thread_registry.mark_dead(tid);
        shared.gc_barrier.mark_blocked_region_leave();
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
        set_process_vm(&shared);

        let _raw = attach_foreign_thread(&shared, false, None);
        with_foreign_thread(|jt| {
            jt.gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release)
        });
        let _ = shared.gc_barrier.mark_blocked_region_enter();
        assert_eq!(shared.gc_barrier.blocked_count(), 1);

        {
            // Outermost call → leave blocked region, counted mutator.
            let _fg = ForeignCallGuard::enter();
            assert_eq!(
                shared.gc_barrier.blocked_count(),
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
                assert_eq!(shared.gc_barrier.blocked_count(), 0);
                assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 2);
            }
            assert_eq!(FOREIGN_CALL_DEPTH.with(|c| c.get()), 1);
        }
        // Outermost return → idle/blocked again.
        assert_eq!(
            shared.gc_barrier.blocked_count(),
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
        shared.thread_registry.mark_dead(tid);
        shared.gc_barrier.mark_blocked_region_leave();
        assert!(detach_foreign_thread(&shared));
        {
            let _fg = ForeignCallGuard::enter();
            assert_eq!(shared.gc_barrier.blocked_count(), 0);
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
        set_process_vm(&shared);

        let init = shared.thread_registry.next_thread_id();
        shared.thread_registry.register(init, "init", None);
        let coord = shared.thread_registry.next_thread_id();
        shared.thread_registry.register(coord, "coordinator", None);
        assert_eq!(shared.thread_registry.alive_count(), 2);

        // This thread declares itself in-native (no foreign attachment present).
        assert!(host_thread_enter_native());
        assert_eq!(shared.gc_barrier.blocked_count(), 1);

        // A STW from the initiator excludes the in-native thread → waits for nobody.
        assert!(shared.gc_barrier.request_stw(init, 2));
        assert_eq!(
            shared.gc_barrier.pending_count(),
            0,
            "in-native thread must be excluded from the STW expected-set"
        );
        shared.gc_barrier.wait_for_all();
        shared.gc_barrier.complete_gc(HashMap::new());

        assert!(host_thread_leave_native());
        assert_eq!(shared.gc_barrier.blocked_count(), 0);
    }

    #[test]
    fn jni_version_constant() {
        assert_eq!(JNI_VERSION_1_8, 0x00010008);
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

        set_jni_context(&vm.shared);
        set_jni_thread(vm.main_thread.as_mut() as *mut _);
        assert_eq!(
            jni_throw_new(get_jni_env(), class_id.as_u32() as JClass, message.as_ptr(),),
            JNI_OK
        );

        let occurred = jni_exception_occurred(get_jni_env());
        assert_ne!(occurred, 0, "ThrowNew must set a pending exception");
        let exception = vm
            .main_thread
            .native_pending_return
            .expect("ThrowNew must publish a GC-rooted exception");
        assert_eq!(
            vm.shared.heap.class_id_of(exception),
            class_id,
            "ThrowNew must preserve the supplied jclass"
        );

        let Value::Object(Some(message_object)) = vm.shared.heap.get_field(exception, 1) else {
            panic!("ThrowNew exception must retain its detailMessage");
        };
        assert_eq!(
            read_java_string(&vm.shared.heap, message_object).as_deref(),
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
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let obj2 = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let local1 = obj_to_jobject(obj1);
        let local2 = obj_to_jobject(obj2);
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(25) };
        let is_same: extern "C" fn(JNIEnv, JObject, JObject) -> JBoolean =
            unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(is_same(env, local1, local1), JNI_TRUE);
        assert_eq!(is_same(env, local1, local2), JNI_FALSE);
        assert_eq!(is_same(env, 0, 0), JNI_TRUE); // null == null
        clear_jni_context();
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
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let obj2 = shared
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
        let obj = shared.heap.alloc_object(ClassId::new(0), 3);
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
        register_jni_native("com/example/Foo", "bar", "()J", fn_ptr);
        assert_eq!(
            find_jni_native("com/example/Foo", "bar", "()J"),
            Some(fn_ptr)
        );
        // Different class → not found
        assert!(find_jni_native("com/example/Other", "bar", "()J").is_none());
        // Different descriptor → not found
        assert!(find_jni_native("com/example/Foo", "bar", "()V").is_none());
    }

    #[test]
    fn register_jni_native_overwrites() {
        extern "C" fn v1(_env: JNIEnv, _this: JObject) -> u64 {
            1
        }
        extern "C" fn v2(_env: JNIEnv, _this: JObject) -> u64 {
            2
        }
        register_jni_native("com/example/Baz", "quux", "()I", v1 as *const () as usize);
        register_jni_native("com/example/Baz", "quux", "()I", v2 as *const () as usize);
        assert_eq!(
            find_jni_native("com/example/Baz", "quux", "()I"),
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
    fn jni_bare_varargs_slots_raise_unsatisfied_link() {
        // The bare C-varargs `...` call slots cannot be dispatched in stable
        // Rust. They must be wired to `jni_varargs_unsupported` (which raises
        // UnsatisfiedLinkError), NOT to the silent `jni_stub` (which would
        // fabricate a 0/null return that a native would mistake for a result).
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        let varargs_ptr = jni_varargs_unsupported as *const () as usize;
        // Instance, nonvirtual, and static bare-varargs slot bases.
        let bare_varargs_slots = [
            34, 37, 40, 43, 46, 49, 52, 55, 58, 61, // CallXxxMethod(...)
            64, 67, 70, 73, 76, 79, 82, 85, 88, 91, // CallNonvirtualXxxMethod(...)
            114, 117, 120, 123, 126, 129, 132, 135, 138, 141, // CallStaticXxxMethod(...)
        ];
        for slot in bare_varargs_slots {
            let func_ptr = unsafe { *(*env).add(slot) };
            assert_ne!(
                func_ptr, stub_ptr,
                "bare-varargs slot {slot} must not be the silent stub"
            );
            assert_eq!(
                func_ptr, varargs_ptr,
                "bare-varargs slot {slot} must raise UnsatisfiedLinkError"
            );
        }
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
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let handle = shared.jni_global_refs.lock().add(obj);
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
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        // Create a global ref via the shared state.
        shared.jni_global_refs.lock().add(obj);
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
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        // Simulate GC moving the object to a new address.
        let old_addr = obj.as_ptr() as usize;
        let fake_new_addr = old_addr.wrapping_add(0x100); // pretend GC moved it
        let mut pointer_map = std::collections::HashMap::new();
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
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let local_ref = obj_to_jobject(obj); // raw local ref
        let global_ref = shared.jni_global_refs.lock().add(obj);
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
        let mut pointer_map = std::collections::HashMap::new();
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
        let mut pointer_map = std::collections::HashMap::new();
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
        cache.insert("(I)V", vec![b'I']);
        assert_eq!(cache.get("(I)V"), Some(&[b'I'][..]));
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
        cache.insert("(I0)V", vec![b'J']);
        assert_eq!(cache.map.len(), DESCRIPTOR_CACHE_CAPACITY);
        assert_eq!(cache.get("(I0)V"), Some(&[b'J'][..]));
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

    #[test]
    fn jni_function_table_extended_to_234() {
        let env = get_jni_env();
        // Table must have at least 234 slots.
        // We check that slot 233 (GetModule) is not a null pointer.
        let func_ptr = unsafe { *(*env).add(233) };
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(
            func_ptr, stub_ptr,
            "slot 233 (GetModule) should not be stub"
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
        assert_ne!(
            unsafe { *(*env).add(233) },
            stub_ptr,
            "slot 233 (GetModule)"
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
}
