#[allow(unused_imports)]
use super::*;


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
/// `JNI_VERSION_24` of JDK 25's `jni.h`: what HotSpot 25's `GetVersion`
/// answers (its `CurrentVersion`, raised with `GetStringUTFLengthAsLong`, slot
/// 235, which this table wires).
pub const JNI_VERSION_24: JInt = 0x0018_0000;
pub const JNI_OK: JInt = 0;
pub const JNI_ERR: JInt = -1;
/// `JNI_EDETACHED`: `GetEnv` from an OS thread that is not attached to the VM
/// (gcf f2/j, [`current_os_thread_is_vm_thread`]).
pub const JNI_EDETACHED: JInt = -2;
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
pub(in crate::native::jni) const DESCRIPTOR_CACHE_CAPACITY: usize = 1024;

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
pub(in crate::native::jni) struct DescriptorCache {
    /// descriptor string → (parsed param-type tags, last-access tick)
    pub(in crate::native::jni) map: HashMap<String, (Vec<u8>, u64)>,
    /// Monotonically increasing access counter; larger = more recently used.
    pub(in crate::native::jni) tick: u64,
}

impl DescriptorCache {
    pub(in crate::native::jni) fn new() -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
        }
    }

    /// Look up `descriptor`, marking it most-recently-used on a hit.
    pub(in crate::native::jni) fn get(&mut self, descriptor: &str) -> Option<&[u8]> {
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
    pub(in crate::native::jni) fn insert(&mut self, descriptor: &str, types: Vec<u8>) {
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
pub(in crate::native::jni) struct ArrayElemBuffer {
    /// Number of elements actually initialised — bounds the copy-back loop so
    /// we never read an uninitialised tail.
    pub(in crate::native::jni) len: usize,
    /// Original `Vec` capacity — MUST be passed to `Vec::from_raw_parts` for a
    /// sound free.
    pub(in crate::native::jni) cap: usize,
    /// Remappable handle to the source array (a JNI global ref, bit 0 set).
    /// Resolved at Release to the array's post-GC location; deleted on the
    /// final (freeing) Release.
    pub(in crate::native::jni) array_gref: JObject,
}

thread_local! {
    /// Holds an `Arc<SharedVm>` while inside a JNI native call.
    ///
    /// Using `Arc` instead of a raw pointer prevents use-after-free: the
    /// reference count keeps the `SharedVm` alive for the entire duration of the
    /// native call, even if other `Arc` holders drop their references.
    pub(in crate::native::jni) static JNI_SHARED_VM: std::cell::RefCell<Option<Arc<SharedVm>>> =
        std::cell::RefCell::new(None);
    pub(in crate::native::jni) static JNI_THREAD: Cell<*mut ()> = const { Cell::new(std::ptr::null_mut()) };
    pub(in crate::native::jni) static JNI_PENDING_EXCEPTION: Cell<u64> = const { Cell::new(0) };
    /// Generation counter incremented on every set/clear cycle.
    /// Allows detecting stale context in nested native calls.
    pub(in crate::native::jni) static JNI_CONTEXT_GENERATION: Cell<u64> = const { Cell::new(0) };
    // The `GetStringChars` and `Get<Type>ArrayElements` records that lived
    // here (`JNI_STRING_BUFFERS`, `JNI_ARRAY_ELEM_BUFFERS`) are per VM since
    // gc-common w10-c: `JniGlobalRefs::{elem_copies, string_copies}`. The
    // `GetPrimitiveArrayCritical` records (`JNI_CRITICAL_COPIES`) followed in
    // gci i13/jc (F10): `JniGlobalRefs::critical_copies`, so a thread that
    // exits inside a section no longer drops the record with its keep-alive
    // global still in the table.
    /// Thread-local cache for parsed method descriptors.
    /// Maps descriptor string → parsed parameter type tags, avoiding
    /// repeated parsing of the same descriptor in hot JNI call paths.
    pub(in crate::native::jni) static JNI_DESCRIPTOR_CACHE: std::cell::RefCell<DescriptorCache> =
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
    let arc = shared_vm_arc(shared);
    let prev = JNI_SHARED_VM.with(|c| (*c.borrow_mut()).replace(arc));
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
    prev
}

/// gcf f4/j (`gcd-d5f-jni-default-dispatch-costs-500ns` item 4): an `Arc` of
/// `shared` without `SharedVm::get_arc`'s `self_arc` `RwLock` read pair. This
/// OS thread keeps a `Weak` of the last VM it installed
/// (`NativeCallTls::vm_weak`) and upgrades that: one CAS on the strong count
/// instead of the lock word's two RMWs plus the CAS -- the lock word is a line
/// every JNI thread of the VM shares on every dispatch. A `Weak` keeps no VM
/// alive (`process_vm`'s registry and every drop are unchanged); a different
/// VM, or a dead one, takes `get_arc` and re-records.
pub(in crate::native::jni) fn shared_vm_arc(shared: &SharedVm) -> Arc<SharedVm> {
    let cached = JNI_NATIVE_CALL
        .try_with(|t| {
            let slot = t.vm_weak.try_borrow().ok()?;
            let weak = slot.as_ref()?;
            if std::ptr::eq(weak.as_ptr(), shared) {
                weak.upgrade()
            } else {
                None
            }
        })
        .ok()
        .flatten();
    if let Some(arc) = cached {
        return arc;
    }
    let arc = shared.get_arc();
    let _ = JNI_NATIVE_CALL.try_with(|t| {
        if let Ok(mut slot) = t.vm_weak.try_borrow_mut() {
            *slot = Some(Arc::downgrade(&arc));
        }
    });
    arc
}

/// gcf f4/j (`gcf-f1j-deposit-scans-fill-never-read-thread-local-movable-sets`):
/// marks a root DEPOSIT (`NativeContextImpl::deposit_root_snapshot_inner`,
/// every flavour: blocking, wake refresh, the JNI package's in-native one) on
/// this OS thread for its duration. A deposit runs on the depositing thread,
/// never inside that thread's own collection's root gathering, so the band
/// scan's movable claims / unrewritable vetoes it would insert into this
/// thread's sets are never read: `collect_roots` clears both before the
/// thread's own scan refills them
/// (`conservative_roots::note_movable_band_root`). Nests; unwind-safe.
#[must_use = "the scope ends when the guard drops"]
pub(crate) struct RootDepositScope(pub(in crate::native::jni) bool);

impl RootDepositScope {
    pub(crate) fn enter() -> Self {
        let prev = JNI_NATIVE_CALL
            .try_with(|t| t.root_deposit.replace(true))
            .unwrap_or(false);
        RootDepositScope(prev)
    }
}

impl Drop for RootDepositScope {
    fn drop(&mut self) {
        let prev = self.0;
        let _ = JNI_NATIVE_CALL.try_with(|t| t.root_deposit.set(prev));
    }
}

/// gcf f4/j: is a root deposit running on this thread ([`RootDepositScope`])?
#[inline]
pub(crate) fn root_deposit_running() -> bool {
    JNI_NATIVE_CALL
        .try_with(|t| t.root_deposit.get())
        .unwrap_or(false)
}

/// gcj-j1pn-young-pin-ledger-review-findings item 4: marks a SAFEPOINT root
/// publish (`gc_and_alloc::update_root_snapshot`) on this OS thread for its
/// duration -- the one scan [`RootDepositScope`]'s argument left out. Its band
/// scan's movable claims and unrewritable vetoes go into this thread's
/// thread-local sets, and no reader ever sees them: a PEER's publish (at its
/// park) fills sets only that peer's own later collection reads, after its
/// `collect_roots` cleared them; the INITIATOR's publish (the pause door's own
/// `update_root_snapshot`, and ZGC's mark-start refresh) runs before the same
/// pause's `collect_roots` clears them. So the inserts were dead -- one hash
/// insert per object word, per park, growing until the thread next collected.
/// `conservative_roots::note_movable_band_root` skips them inside the scope
/// (`CRATONVM_GC_SAFEPOINT_PUBLISH_SKIPS_BAND_SETS=0` keeps them: the scope
/// then marks nothing). Nests; unwind-safe.
#[must_use = "the scope ends when the guard drops"]
pub(crate) struct SafepointPublishScope(pub(in crate::native::jni) bool);

impl SafepointPublishScope {
    /// Reads the switch once per publish, never per band word.
    pub(crate) fn enter() -> Self {
        let skip = true;
        let prev = JNI_NATIVE_CALL
            .try_with(|t| t.safepoint_publish.replace(skip))
            .unwrap_or(false);
        SafepointPublishScope(prev)
    }
}



impl Drop for SafepointPublishScope {
    fn drop(&mut self) {
        let prev = self.0;
        let _ = JNI_NATIVE_CALL.try_with(|t| t.safepoint_publish.set(prev));
    }
}

/// gcj j1pn ledger item 4: is a safepoint root publish running on this thread
/// ([`SafepointPublishScope`])?
#[inline]
pub(crate) fn safepoint_publish_running() -> bool {
    JNI_NATIVE_CALL
        .try_with(|t| t.safepoint_publish.get())
        .unwrap_or(false)
}

/// gcf f4/j: what [`enter_jni_context`] left to undo.
#[must_use = "leave_jni_context puts the previous context back"]
pub enum JniContextSave {
    /// The thread already had THIS VM installed (a nested dispatch): nothing
    /// was swapped, no `Arc` was cloned, and nothing is put back.
    Kept,
    /// [`replace_jni_context`]'s answer, put back by [`restore_jni_context`].
    Replaced(Option<Arc<SharedVm>>),
}

/// gcf f4/j: [`replace_jni_context`] for `JniContextGuard`: a nested dispatch
/// into the VM already installed on this thread keeps that `Arc` where it is
/// (the state it would restore is the state it finds), so it pays no
/// refcount traffic at all. The generation still moves twice per call, as
/// before (`with_shared_vm`'s nesting note reads it).
pub fn enter_jni_context(shared: &SharedVm) -> JniContextSave {
    let same = JNI_SHARED_VM.with(|c| {
        c.try_borrow()
            .ok()
            .and_then(|b| b.as_ref().map(|a| std::ptr::eq(Arc::as_ptr(a), shared)))
            .unwrap_or(false)
    });
    if same {
        JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
        return JniContextSave::Kept;
    }
    JniContextSave::Replaced(replace_jni_context(shared))
}

/// gcf f4/j: undo [`enter_jni_context`].
pub fn leave_jni_context(save: JniContextSave) {
    match save {
        JniContextSave::Kept => {
            JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
        }
        JniContextSave::Replaced(prev) => restore_jni_context(prev),
    }
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
    // gci i9/jw: every JNI native dispatch installs its thread here
    // (`JniContextGuard::install`); publish where this OS thread keeps the
    // thread's JNI locals, for a take-over that freezes it
    // (`push_frozen_peer_local_refs`). Only a shared read of one field.
    if !thread.is_null() {
        // SAFETY: the caller's contract: `thread` is a valid `JvmThread`.
        publish_local_frames_owner(&(*thread).gc_block_state);
    }
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
pub(in crate::native::jni) static PROCESS_VM: parking_lot::Mutex<Option<Weak<SharedVm>>> = parking_lot::Mutex::new(None);

/// Publish the process-global VM so foreign threads can resolve it from a
/// `JavaVM*` in `AttachCurrentThread`. Called once by `JNI_CreateJavaVM`
/// (and `cratonvm_create`) right after the JNI TLS context is set. Idempotent
/// — a later create (e.g. a test that builds a second `Vm` in-process) replaces
/// the cell; the previous `Weak` simply stops upgrading once its `Arc` is gone.
pub(in crate::native::jni) struct RegisteredVm {
    pub(in crate::native::jni) weak: Weak<SharedVm>,
    /// Heap-allocated `Box<usize>` holding the JNI invoke-table pointer for
    /// this VM. Its address (`java_vm_slot as JavaVM`) is this VM's unique
    /// `JavaVM*` handle (`*const *const usize`) and is freed in `Drop` when the
    /// dead `Weak` is pruned.
    pub(in crate::native::jni) java_vm_slot: *mut usize,
}

// SAFETY: `java_vm_slot` points to a `Box<usize>` owned exclusively by this
// `RegisteredVm` entry, never mutated after creation, and freed in `Drop` once
// the entry is pruned or replaced.
unsafe impl Send for RegisteredVm {}

impl Drop for RegisteredVm {
    fn drop(&mut self) {
        if !self.java_vm_slot.is_null() {
            // SAFETY: allocated with `Box::into_raw(Box::new(...))` in
            // `register_vm_locked`.
            unsafe {
                drop(Box::from_raw(self.java_vm_slot));
            }
        }
    }
}

pub(in crate::native::jni) fn register_vm_locked(reg: &mut Vec<RegisteredVm>, shared: &Arc<SharedVm>) -> JavaVM {
    reg.retain(|e| e.weak.strong_count() > 0);
    let weak = Arc::downgrade(shared);
    if let Some(existing) = reg.iter().find(|e| e.weak.ptr_eq(&weak)) {
        return existing.java_vm_slot.cast();
    }
    let invoke_ptr = JNI_INVOKE_TABLE_PTR.load(std::sync::atomic::Ordering::Acquire) as usize;
    let slot = Box::into_raw(Box::new(invoke_ptr));
    reg.push(RegisteredVm {
        weak,
        java_vm_slot: slot,
    });
    slot.cast()
}

pub fn set_process_vm(shared: &Arc<SharedVm>) {
    // Round 13 wave 1 (lane mhffm patch): FFM upcalls on foreign threads.
    cratonvm_native_builtins::panama_upcall::set_upcall_attach_hook(ffm_upcall_attach);
    init_jni_table();
    let mut cell = PROCESS_VM.lock();
    *cell = Some(Arc::downgrade(shared));
    drop(cell);
    let mut reg = VM_REGISTRY.lock();
    register_vm_locked(&mut reg, shared);
}

/// Return the per-VM `JavaVM*` handle for `shared`, registering `shared` if
/// not already in `VM_REGISTRY`.
pub fn java_vm_for(shared: &Arc<SharedVm>) -> JavaVM {
    init_jni_table();
    let mut reg = VM_REGISTRY.lock();
    register_vm_locked(&mut reg, shared)
}

/// Resolve the live VM that owns `vm` (`JavaVM*`).
///
/// Falls back to [`process_vm`] only when `vm` is the legacy process-global
/// invoke-table slot (`&JNI_INVOKE_TABLE_PTR`). Any other `JavaVM*` must match
/// a live VM in `VM_REGISTRY` — a stale or unknown handle returns `None` rather
/// than attaching to another VM.
pub fn vm_for_java_vm(vm: JavaVM) -> Option<Arc<SharedVm>> {
    if vm.is_null() {
        return None;
    }
    let matched = {
        let mut reg = VM_REGISTRY.lock();
        reg.retain(|e| e.weak.strong_count() > 0);
        reg.iter()
            .find(|e| std::ptr::eq(e.java_vm_slot.cast::<*const usize>(), vm))
            .map(|e| e.weak.clone())
    };
    if let Some(weak) = matched {
        return weak.upgrade();
    }
    if vm == std::ptr::addr_of!(JNI_INVOKE_TABLE_PTR).cast() {
        return process_vm();
    }
    None
}

/// Every live VM in the process, not just the most recently published one.
///
/// `PROCESS_VM` is a single cell that `Vm::new` overwrites unconditionally, so
/// on its own it cannot answer "is there more than one VM here?" — and every
/// caller of [`process_vm`] was silently getting whichever VM happened to be
/// created last. This registry exists so that question is answerable, and so
/// the callers that must not guess can refuse instead.
pub(in crate::native::jni) static VM_REGISTRY: parking_lot::Mutex<Vec<RegisteredVm>> = parking_lot::Mutex::new(Vec::new());

/// Number of live VMs in this process.
pub fn live_vm_count() -> usize {
    let mut reg = VM_REGISTRY.lock();
    reg.retain(|e| e.weak.strong_count() > 0);
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

pub(in crate::native::jni) static AMBIGUOUS_RESOLUTIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`process_vm`], but `None` when the answer is ambiguous.
///
/// Use this wherever picking the wrong VM is worse than doing nothing. The JIT
/// safepoint slow path is the motivating case: parking against another VM's
/// stop-the-world barrier is a hang or worse, while declining to park merely
/// leaves this poll ineffective.
pub fn process_vm_strict() -> Option<Arc<SharedVm>> {
    let mut reg = VM_REGISTRY.lock();
    reg.retain(|e| e.weak.strong_count() > 0);
    if reg.len() != 1 {
        return None;
    }
    reg[0].weak.upgrade()
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
        reg.retain(|e| e.weak.strong_count() > 0);
        reg.iter().map(|e| e.weak.clone()).collect()
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
pub(in crate::native::jni) static DESTROY_VM_HOOK: parking_lot::Mutex<Option<fn() -> JInt>> = parking_lot::Mutex::new(None);

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
pub(in crate::native::jni) fn run_destroy_vm_hook() -> JInt {
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
    pub(in crate::native::jni) static FOREIGN_THREAD_BOX: std::cell::RefCell<Option<ForeignThreadBox>> =
        const { std::cell::RefCell::new(None) };
    /// Depth of nested Java calls in flight on a foreign-attached thread. Used
    /// to scope the idle-attached blocked-region transition (§3.3) to the
    /// OUTERMOST Java call: `0` means the thread is idle (between calls, parked
    /// in the host event loop) and is modelled as GC-blocked.
    pub(in crate::native::jni) static FOREIGN_CALL_DEPTH: Cell<u32> = const { Cell::new(0) };
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
    pub(in crate::native::jni) static FOREIGN_CRASH_FRAMES: std::cell::RefCell<
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
    pub(in crate::native::jni) static FOREIGN_ATTACH_VM: std::cell::RefCell<Option<Weak<SharedVm>>> =
        const { std::cell::RefCell::new(None) };
    /// gc-common w10-c (`CRATONVM_JNI_FOREIGN_TRANSITIONS`): the depth of the
    /// attach-level local frame [`attach_foreign_thread`] opened, closed by
    /// [`detach_foreign_thread`]. `None` with the flag off.
    pub(in crate::native::jni) static FOREIGN_ATTACH_FRAME: Cell<Option<usize>> = const { Cell::new(None) };
    /// Interpreter round i4 wave 84, lane L6: the registry id of this OS
    /// thread's foreign attachment when `AttachCurrentThread` gave it NO name,
    /// until its `java.lang.Thread` mirror is built and takes its `Thread-N`
    /// name from the JDK's counter ([`take_foreign_default_name`]).
    pub(in crate::native::jni) static FOREIGN_DEFAULT_NAME: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The VM this OS thread's foreign attachment belongs to (gc-common w10-c,
/// [`FOREIGN_ATTACH_VM`]). Falls back to the thread's JNI context and then to
/// `process_vm()` only when the attachment recorded no VM. `try_with`
/// throughout: the detach path can run during TLS teardown.
pub(in crate::native::jni) fn foreign_attachment_vm() -> Option<Arc<SharedVm>> {
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

/// Explicit thread owner without a process fallback, for diagnostic attribution.
pub(crate) fn calling_thread_vm_only() -> Option<Arc<SharedVm>> {
    JNI_SHARED_VM
        .try_with(|c| c.try_borrow().ok().and_then(|b| b.clone()))
        .ok()
        .flatten()
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
    pub(in crate::native::jni) jt: std::mem::ManuallyDrop<Box<JvmThread>>,
    /// `true` while the registry entry is live and nothing has detached it:
    /// dropping the box then has to reap the entry first.
    pub(in crate::native::jni) armed: bool,
    /// The VM the entry is registered in (`None`: the attaching `SharedVm`
    /// had no `self_arc`, so no reap is possible and an armed box is leaked).
    pub(in crate::native::jni) vm: Option<Weak<SharedVm>>,
    /// gcd d4/k2 (lane m's request 1): the pause ledger this attachment is
    /// counted in as holding RAW JNI locals
    /// (`gc_quiescence::note_raw_jni_locals_open`), when it is; closed exactly
    /// once, by the detach or by this box's drop (an OS thread that exits
    /// attached holds nothing any more, whether it is reaped or leaked).
    pub(in crate::native::jni) raw_locals_ledger: Option<Arc<cratonvm_gc::gc_quiescence::PauseLedger>>,
}

impl ForeignThreadBox {
    /// A box for an attachment registered in `vm`'s registry.
    pub(in crate::native::jni) fn attached(jt: Box<JvmThread>, vm: Option<Weak<SharedVm>>) -> Self {
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
    pub(in crate::native::jni) fn unregistered(jt: Box<JvmThread>) -> Self {
        ForeignThreadBox {
            jt: std::mem::ManuallyDrop::new(jt),
            armed: false,
            vm: None,
            raw_locals_ledger: None,
        }
    }

    /// Close this attachment's raw-JNI-locals count, if it holds one.
    pub(in crate::native::jni) fn close_raw_locals(&mut self) {
        if let Some(ledger) = self.raw_locals_ledger.take() {
            cratonvm_gc::gc_quiescence::note_raw_jni_locals_closed(&ledger);
        }
    }

    /// The `JvmThread`, for a detach that has done (or is about to do) the
    /// registry half itself: disarms the reaping destructor.
    pub(in crate::native::jni) fn into_detached(mut self) -> Box<JvmThread> {
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
pub(in crate::native::jni) fn reap_undetached_foreign_thread(shared: &SharedVm, jt: &mut JvmThread) -> bool {
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
// under `--real-jdk`, every one of them *after* the class had already printed
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

/// [`attach_foreign_thread_registered`], counting the attachment in the
/// JMM's started / live / peak thread counters (`jmm_counted`: every JNI
/// attach and every other service) or not. zt t11/tg4 (`zgc-zt10tg3-*` item
/// 1): the ZGC director tick's collection thread passes `false`, because
/// HotSpot's `ZDirector` is a `NonJavaThread` that `ThreadMXBean` never
/// counts; its detach then takes nothing off the live count
/// (`ThreadRegistry::jmm_note_death` reads the entry's own `jmm_counted`).
pub(in crate::native::jni) fn attach_foreign_thread_registered_as(
    shared: &SharedVm,
    daemon: bool,
    name: Option<&str>,
    stw_ready: bool,
    jmm_counted: bool,
) -> *mut JvmThread {
    let tid = shared.threads.thread_registry.next_thread_id();
    // Caller-supplied name (from `JavaVMAttachArgs.name`) when present.
    // Otherwise HotSpot's `JavaThread::allocate_threadObj` runs
    // `Thread(ThreadGroup, Runnable)`, so the name is `"Thread-" +
    // ThreadNumbering.next()` -- the JDK's counter, shared with every unnamed
    // `new Thread()`. Interpreter round i4 wave 84, lane L6: this used the
    // VM's registry id (`Thread-<tid>`), a number from another sequence that
    // can repeat a Java-created thread's name. The placeholder below names
    // the registry entry until the lazy mirror is built; that build takes the
    // JDK's name and renames the entry (`take_foreign_default_name`).
    // An EMPTY name is a name (HotSpot passes it to `Thread(ThreadGroup,
    // String)` as given), so only a missing one defaults.
    let unnamed = name.is_none();
    let name = match name {
        Some(n) => n.to_string(),
        None => format!("Thread-{}", tid.0),
    };
    let _ = FOREIGN_DEFAULT_NAME.try_with(|c| c.set(unnamed.then_some(tid.0)));
    let mut jt = Box::new(JvmThread::new(tid, &name));
    // nio21 wave 3 L1: this VM's redefinitions only.
    jt.invoke_cache
        .bind_redefinition_log(shared.classes.redefinition_log.clone());
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
    // Interpreter round i5 wave 100, lane L6: an attached thread counts in
    // the JMM's started / live / peak counters (HotSpot's `attach_current_thread`
    // adds it to `ThreadService`); its detach's `mark_dead` takes it out.
    // zt t11/tg4: not for a VM service thread HotSpot keeps out of the JMM
    // (`jmm_counted == false`: the ZGC director tick's collection thread).
    if jmm_counted {
        shared.threads.thread_registry.note_jmm_thread_start(tid);
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
    let base = local_frame_depth();
    push_local_frame(16);
    FOREIGN_ATTACH_FRAME.with(|c| c.set(Some(base)));
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
    // Interpreter round i4 wave 99, lane L6
    // (`CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`; page
    // `i94-L6-thread-death-sweeps-every-inflated-monitor`): HotSpot's
    // `DetachCurrentThread` -> `JavaThread::exit` walks the monitors only for
    // a thread whose `_jni_monitor_count` is not 0. A detach has no Java frame
    // left (every up-call returned or threw); one that failed with a VM error
    // instead is remembered on the thread (`vm_error_upcall_seen`) and keeps
    // the walk. Read while the registry entry (its `MonitorEnter` count) is
    // alive.
    let skip_death_sweep =
        crate::vm::jdk_continuation::platform_death_may_skip_sweep(shared, &jt, true);
    // Drop out of `alive_count` / STW `expected` before reclaiming the TLAB so a
    // subsequent `request_stw` no longer waits for this thread.
    shared.threads.thread_registry.mark_dead(tid);
    // A detached host thread may remain alive forever. Its JNI local pins
    // must end with the attachment, rather than waiting for TLS destruction.
    cratonvm_gc::gc_quiescence::clear_pinned_jit_roots();
    // A thread torn down while blocked inside a native call made from within
    // a `synchronized` region never executes its `monitorexit` bytecode —
    // release anything it still holds so no future locker waits forever
    // (see `MonitorTable::release_monitors_held_by`). Wave 99: unless the
    // count above says it holds none (the thin-lock slot goes back anyway).
    crate::vm::jdk_continuation::platform_death_releases_monitors(
        shared,
        tid,
        None,
        skip_death_sweep,
    );
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
    // gci i5/fj: the idle record named this attachment's snapshot.
    forget_foreign_idle_record();
    // gc-common w10-c: close the attach-level local frame (and anything a
    // native pushed on top of it and never popped), and forget the VM.
    if let Some(base) = FOREIGN_ATTACH_FRAME.try_with(Cell::take).ok().flatten() {
        truncate_local_frames(base);
    }
    let _ = FOREIGN_ATTACH_VM.try_with(|c| c.try_borrow_mut().map(|mut w| w.take()));
    let _ = FOREIGN_DEFAULT_NAME.try_with(|c| c.set(None));
    true
}

/// `true` once when `tid` is this OS thread's foreign attachment and
/// `AttachCurrentThread` gave it no name: its lazy `java.lang.Thread` mirror
/// must take `Thread.genThreadName()`, as HotSpot's attach does
/// (`JavaThread::allocate_threadObj` with a null name runs
/// `Thread(ThreadGroup, Runnable)`). Cleared by the first ask that matches.
pub(crate) fn take_foreign_default_name(tid: ThreadId) -> bool {
    FOREIGN_DEFAULT_NAME
        .try_with(|c| {
            if c.get() == Some(tid.0) {
                c.set(None);
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
}

/// Run `f` against this OS thread's foreign-attached `JvmThread`, if any.
///
/// Safe to call only at points where the interpreter does NOT also hold the
/// `&mut JvmThread` (which it borrows from the raw `JNI_THREAD` pointer): namely
/// the attach/detach paths and the foreign-call transitions, which run strictly
/// before a call begins or after it returns — never concurrently with the call.
pub(in crate::native::jni) fn with_foreign_thread<R>(f: impl FnOnce(&mut JvmThread) -> R) -> Option<R> {
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
pub(in crate::native::jni) fn release_foreign_thread_locals() {
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
    // gci i18/na: not under `safe_native_call_impl`, and reached from the
    // agent's `DetachCurrentThread` (C frames above): the release only drops
    // roots, but `current_thread_object` mints a missing mirror, so it runs
    // inside the scope -- a refusal unwinds no further than here and leaves
    // the rows to the VM's teardown, instead of the infallible arm's abort.
    let _ = with_jni_context(|shared, thread| {
        crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
            cratonvm_native_builtins::phases_early::release_current_thread_locals(
                &mut crate::vm::NativeContextImpl { shared, thread },
            )
        })
    });
}

/// The `java.lang.Thread` of a thread `AttachCurrentThread` has just attached
/// (interpreter round i4 wave 85, lane L6), built before the attach returns
/// as HotSpot's `attach_current_thread` builds it (`allocate_threadObj`): in
/// `group` (a `JavaVMAttachArgs.group` handle; `0` or a non-`ThreadGroup` is
/// the main group), named by `JavaVMAttachArgs.name` or the JDK's `Thread-N`
/// counter at this moment. It used to be built on the thread's first
/// `Thread.currentThread()`, so until then the thread was missing from
/// `Thread.getAllStackTraces()` and its number was drawn late; the group was
/// never honoured. Runs as one Java call would (`ForeignCallGuard`); the
/// handle is resolved inside it and rooted across the build.
pub(in crate::native::jni) fn build_foreign_thread_object(group: JObject) {
    let _fg = ForeignCallGuard::enter();
    let _ = with_jni_context(|shared, thread| {
        let group = if group == 0 { None } else { jobject_to_obj(group) };
        crate::vm::vm_exec::build_attached_thread_object(shared, thread, group);
    });
}

/// The Java side of a `DetachCurrentThread` (interpreter round i4 wave 84,
/// lane L6): HotSpot's `jni_DetachCurrentThread` runs `JavaThread::exit` --
/// `Thread.exit()`, then `ensure_join` (TERMINATED, `eetop` 0, `notifyAll` on
/// the `Thread`) -- so Java that kept the attached thread's `Thread` sees it
/// dead and a `join()` on it returns ([`crate::vm::vm_exec::exit_attached_thread_java_side`]).
/// Same preconditions and transition as [`release_foreign_thread_locals`],
/// which runs right after it; only the JNI detach calls it (the VM's own
/// attached service threads stop at VM teardown, with nothing joining them).
///
/// Interpreter round i5 wave 107 (lane L1): one half per call, `join` false
/// for `Thread.exit()` and true for `ensure_join`, each its own Java call
/// (`ForeignCallGuard`), so the detach posts the thread's end events and
/// holds between them while the thread is idle-blocked (GC-safe) as between
/// any two JNI calls.
pub(in crate::native::jni) fn end_foreign_thread_java_side(join: bool) {
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
    if jni_bound_thread_id() != Some(me) {
        return;
    }
    let _fg = ForeignCallGuard::enter();
    let _ = with_jni_context(|shared, thread| {
        if join {
            crate::vm::vm_exec::join_attached_thread_java_side(shared, thread);
        } else {
            // Wave 107 (lane L1): the exception the thread left pending goes
            // to its uncaught handler first, as `JavaThread::exit` does
            // (`exit_attached_thread_java_side`); it was dropped. The
            // GC-remapped root first, as `ExceptionDescribe` reads it (the
            // raw handle may predate a collection); the `u64::MAX` sentinel
            // names no object. Cleared before the dispatch runs Java.
            let pending = JNI_PENDING_EXCEPTION
                .try_with(|cell| cell.replace(0))
                .unwrap_or(0);
            let uncaught = if pending == 0 {
                None
            } else {
                thread.native_pending_return.or_else(|| {
                    (pending != u64::MAX)
                        .then(|| jobject_to_obj(pending))
                        .flatten()
                })
            };
            thread.native_pending_return = None;
            crate::vm::vm_exec::exit_attached_thread_java_side(shared, thread, uncaught);
        }
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
    // gcd d10/j (`docs/feature-designs/real-jdk-mode.md`):
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
pub(in crate::native::jni) fn aio_dispatch_thread_count() -> usize {
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
pub(in crate::native::jni) fn start_aio_dispatcher_for(shared: &Arc<SharedVm>) {
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
pub(in crate::native::jni) fn live_vm_by_identity(vm: usize) -> Option<Arc<SharedVm>> {
    let weaks: Vec<Weak<SharedVm>> = VM_REGISTRY.lock().iter().map(|e| e.weak.clone()).collect();
    weaks
        .iter()
        .filter_map(Weak::upgrade)
        .find(|s| s.vm_identity == vm)
}

pub(in crate::native::jni) fn aio_dispatcher_main(shared: Arc<SharedVm>) {
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
        // gci i18/na: this context is not under `safe_native_call_impl`, so
        // the drain's Rust-side allocations (the boxed `Integer` result, the
        // `IOException` and its message, an accepted channel) used to take
        // the infallible arm, which aborts the process on Generational and
        // ZGC. Inside the scope a refusal fails this drain instead: the
        // completion being delivered is lost (its handler is not called, as
        // on HotSpot, where the `OutOfMemoryError` ends the handler's task on
        // the group's thread) and the loop goes on serving the queue.
        let _ = with_jni_context(|shared, thread| {
            let drained = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                cratonvm_native_io::async_socket::drain_completions_pub(&mut ctx);
            });
            if let Err(oom) = drained {
                tracing::error!(
                    "AIO completion drain failed: java.lang.OutOfMemoryError: {}",
                    oom.java_message()
                );
            }
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
    run_attached_service_as(shared, name, true, started, jobs);
}

/// [`run_attached_service`], with the attachment counted in the JMM's
/// thread counters (`jmm_counted`) or not. zt t11/tg4 (`zgc-zt10tg3-*` item
/// 1, `CRATONVM_ZGC_TICK_THREAD_UNCOUNTED`): the ZGC director tick's
/// collection passes `false`, so a `Timer` cycle on an idle heap moves none
/// of `ThreadMXBean.getTotalStartedThreadCount()` / `getThreadCount()` /
/// `getPeakThreadCount()`, as HotSpot's `NonJavaThread` director moves none.
/// The attachment is still a registered, stop-the-world-visible mutator for
/// its jobs (it must initiate the pause).
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn run_attached_service_as(
    shared: Arc<SharedVm>,
    name: &str,
    jmm_counted: bool,
    started: std::sync::mpsc::SyncSender<u64>,
    jobs: std::sync::mpsc::Receiver<AttachedServiceJob>,
) {
    attach_foreign_thread_idle_as(shared, true, Some(name), jmm_counted);
    let tid = with_foreign_thread(|jt| jt.thread_id.0).unwrap_or(0);
    let _ = started.send(tid);
    while let Ok(job) = jobs.recv() {
        let _fg = ForeignCallGuard::enter();
        // gci i18/na: the job's context is not under `safe_native_call_impl`
        // (the JDWP heap service's `CreateString` / `NewInstance`, the attach
        // listener's `load` and property serialisation allocate). Inside the
        // scope a refused allocation fails THAT job -- its reply channel is
        // dropped, so the requester sees the job as not run -- instead of
        // the infallible arm's abort, and the service goes on.
        let _ = with_jni_context(|shared, thread| {
            let ran = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
                job(shared, thread)
            });
            if let Err(oom) = ran {
                tracing::warn!(
                    "{name}: a job failed: java.lang.OutOfMemoryError: {}",
                    oom.java_message()
                );
            }
        });
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
    // JIT round 16 wave 12 (lane inpkg): C code inside a JNI native in native
    // (`CRATONVM_JNI_NATIVE_TRANSITIONS`) calling a stub of this VM: leave
    // native first, as a JNIEnv function does -- the up-call's Java used to
    // run with `in_blocked_region` (and `foreign_native`) still up, excluded
    // from every pause. Before `with_jni_context` (it forms the `&mut
    // JvmThread` the leave also needs) and before the guard below.
    let _in_vm = ForeignJniEntry::enter_for_upcall(vm);
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
/// `process_vm`). Also the C JVMTI table's (`jvmti::native_env::class_file_load_hook_vm`).
pub(crate) fn vm_for_identity(vm: usize) -> Option<Arc<SharedVm>> {
    let candidates: Vec<Weak<SharedVm>> = {
        let mut reg = VM_REGISTRY.lock();
        reg.retain(|e| e.weak.strong_count() > 0);
        reg.iter().map(|e| e.weak.clone()).collect()
    };
    candidates
        .iter()
        .filter_map(Weak::upgrade)
        .find(|shared| shared.vm_identity == vm)
}

/// Run `f` while this foreign-attached OS thread is settled in its idle
/// GC-blocked region (`in_blocked_region == true`), so waiting on another
/// thread's STW GC cycle cannot deadlock.
pub(crate) fn with_foreign_idle_safepoint<R>(shared: &SharedVm, f: impl FnOnce() -> R) -> R {
    let needs_idle = with_foreign_thread(|jt| {
        jt.gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
    }) == Some(false);
    if !needs_idle {
        return f();
    }
    struct ForeignIdleReset<'a>(&'a SharedVm);
    impl Drop for ForeignIdleReset<'_> {
        fn drop(&mut self) {
            let _ = foreign_leave_idle(self.0);
        }
    }
    foreign_enter_idle(shared);
    let _reset = ForeignIdleReset(shared);
    f()
}

#[cfg(test)]
pub(crate) fn attach_foreign_thread_idle_for_test(shared: Arc<SharedVm>, name: &str) {
    attach_foreign_thread_idle_as(shared, true, Some(name), false);
}

#[cfg(test)]
pub(crate) fn detach_foreign_thread_idle_for_test() {
    if let Some(shared) = foreign_attachment_vm() {
        detach_foreign_thread_idle(&shared);
    }
    clear_jni_thread();
    clear_jni_context();
}

#[cfg(test)]
pub(crate) fn run_in_foreign_call_guard_for_test<R>(f: impl FnOnce() -> R) -> R {
    let _fg = ForeignCallGuard::enter();
    f()
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
pub(in crate::native::jni) fn attach_foreign_thread_idle(
    shared: Arc<SharedVm>,
    daemon: bool,
    name: Option<&str>,
) -> *mut JvmThread {
    attach_foreign_thread_idle_as(shared, daemon, name, true)
}

/// [`attach_foreign_thread_idle`], counted in the JMM's thread counters or
/// not ([`attach_foreign_thread_registered_as`]; zt t11/tg4).
pub(in crate::native::jni) fn attach_foreign_thread_idle_as(
    shared: Arc<SharedVm>,
    daemon: bool,
    name: Option<&str>,
    jmm_counted: bool,
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
    let raw = attach_foreign_thread_registered_as(&shared, daemon, name, false, jmm_counted);
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
    // gci i9/jw: where this attachment keeps its JNI locals, for a take-over
    // that freezes it in compiled Java (`push_frozen_peer_local_refs`).
    let _ = with_foreign_thread(|jt| publish_local_frames_owner(&jt.gc_block_state));
    set_jni_thread(raw);
    raw
}

/// Tear down this OS thread's idle foreign attachment to `shared`: release its
/// `ThreadLocal` values while it is still registered, retire its TLAB and mark
/// it dead inside one barrier-serialized leave, then reclaim it
/// ([`detach_foreign_thread`]). The caller clears the JNI TLS afterwards.
/// Shared by `DetachCurrentThread` and the AIO dispatcher's shutdown
/// (gc-common w10-c).
pub(in crate::native::jni) fn detach_foreign_thread_idle(shared: &SharedVm) {
    // Drop this attachment's `ThreadLocal` values (their JNI global roots)
    // while it is still registered: `current_thread_object` needs the entry.
    // See `release_foreign_thread_locals`.
    release_foreign_thread_locals();
    let tid = with_foreign_thread(|jt| jt.thread_id);
    // gci i14/pn: the G1 critical pins this attachment left open (a native
    // bug; `CRATONVM_JNI_CRITICAL_G1_PIN` only: no record exists without it).
    if let Some(tid) = tid {
        release_critical_pins_opened_by(shared, tid);
    }
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
pub(in crate::native::jni) fn foreign_attach_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
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
pub(in crate::native::jni) unsafe fn read_attach_args(args: *mut std::ffi::c_void) -> (Option<String>, JObject) {
    if args.is_null() {
        return (None, 0);
    }
    let a = &*(args as *const JavaVMAttachArgs);
    // HotSpot's `attach_current_thread` reads `name` and `group` only when
    // `Threads::is_supported_jni_version(args->version)` -- the `GetEnv`
    // versions minus 1.1 -- and otherwise attaches as if no args were given
    // (no error). Interpreter round i4 wave 84, lane L6.
    if a.version == 0x0001_0001 || !is_supported_jni_version(a.version) {
        return (None, 0);
    }
    // Wave 85 (lane L6): the name is MODIFIED UTF-8, as HotSpot's
    // `java_lang_String::create_from_str` reads it -- a supplementary
    // character arrives as a CESU-8 surrogate pair and `C0 80` is U+0000. It
    // went through strict UTF-8, which refused both and fell back to the
    // default name. An undecodable name still falls back.
    let name = if a.name.is_null() {
        None
    } else {
        decode_modified_utf8(CStr::from_ptr(a.name).to_bytes())
            .map(|units| String::from_utf16_lossy(&units))
    };
    (name, a.group)
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
pub(in crate::native::jni) struct ForeignCallGuard {
    /// `Some(vm)` iff this guard performed the outermost idle→running transition
    /// and must perform the running→idle transition on drop.
    pub(in crate::native::jni) transition: Option<Arc<SharedVm>>,
    /// `true` iff this guard incremented [`FOREIGN_CALL_DEPTH`] (i.e. the thread
    /// is foreign-attached) and must decrement it on drop.
    pub(in crate::native::jni) counted: bool,
    /// The local-frame depth before this guard opened its per-call frame
    /// (meaningful only with `transition`): the drop closes every frame from
    /// there up (gc-common w7-c, [`truncate_local_frames`]).
    pub(in crate::native::jni) frames_base: usize,
}

impl ForeignCallGuard {
    pub(in crate::native::jni) fn enter() -> Self {
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
///
/// gci i5/fj (`CRATONVM_JNI_FOREIGN_QUIET_IDLE=1`, [`foreign_quiet_idle_active`]):
/// the wake is skipped when the leave is QUIET -- nothing completed or folded
/// since the idle deposit ([`ForeignIdleRecord`]) and no Java frame -- and
/// the answer is whether it was (always `false` with the switch off).
pub(in crate::native::jni) fn foreign_leave_idle(shared: &SharedVm) -> bool {
    // gci i4/fj: the foreign transition's phases in the JNI phase census
    // (`CRATONVM_DBG_JNI_PHASE=1`; off, `start` is `None` and every lap and
    // note below is a not-taken branch).
    let census = &shared.natives.jni_phase_census;
    let start = census.start();
    let mut clock = start;
    if census.is_on() && shared.mem.gc_barrier.stop_requested() {
        census.note(JniPhase::ForeignLeaveRaced);
    }
    shared.mem.gc_barrier.mark_blocked_region_leave();
    census.lap(JniPhase::ForeignLeaveBarrier, &mut clock);
    let quiet = with_foreign_thread(|jt| {
        // gcd d3/k (`gcd-d2i-foreign-attached-threads-publish-no-os-tid`,
        // with `CRATONVM_JNI_FOREIGN_TRANSITIONS`): publish this OS thread's
        // id for as long as the attachment runs Java or a JNI function, so
        // the take-over can freeze and scan it in compiled code and a block
        // inside a callback is in the helper-window roster, like every other
        // Java thread. Before the flag clears (no pause is in progress here:
        // the leave above waited it out), so the first pause that counts the
        // thread also lists it.
        // gch h1/jw: with the defaults too (`foreign_tid_publish_active`).
        if foreign_tid_publish_active() {
            shared
                .threads
                .thread_registry
                .republish_os_tid_current(jt.thread_id);
        }
        // gci i5/fj: the quiet leave. `leave_blocked_region_flagged_if` asks
        // under the barrier lock, with no pause in progress and none able to
        // begin, so the answer cannot change before the flag drops (pauses
        // begin only under that lock; folds run only inside pauses). `false`
        // leaves the flag UP for the full wake below, which clears it itself.
        if foreign_quiet_idle_active() && jt.frames.is_empty() {
            if let Some(record) = foreign_idle_record_for(shared.vm_identity, jt.thread_id) {
                let block = &jt.gc_block_state;
                let quiet = shared.mem.gc_barrier.leave_blocked_region_flagged_if(
                    jt.thread_id,
                    &block.in_blocked_region,
                    || NativeSync::read(shared, block) == record.sync,
                );
                if quiet {
                    // What the full wake would still have done with nothing
                    // moved: drop the deposit's slot origins (no frame holds
                    // one: `frames` is empty) and note the heal point
                    // (`JvmThread::last_heal_collection`, diagnostics).
                    jt.gc_block_state.slot_origins.lock().clear();
                    jt.last_heal_collection = shared.mem.heap.collection_count();
                    return true;
                }
            }
        }
        crate::vm::NativeContextImpl { shared, thread: jt }.check_post_block_gc();
        false
    })
    .unwrap_or(false);
    if quiet {
        census.note(JniPhase::ForeignLeaveQuiet);
    }
    census.lap(JniPhase::ForeignWake, &mut clock);
    census.stop(JniPhase::ForeignLeave, start);
    quiet
}

/// Running -> idle for this OS thread's foreign attachment to `shared`: retire
/// the TLAB while still a counted mutator (a pause requested now is waiting
/// for this thread, so the tail filler cannot race its collector), deposit a
/// REAL root snapshot (frames, `java_thread_obj`, the JNI local frames, ...),
/// raise `in_blocked_region`, then re-enter the blocked region. See
/// `ForeignCallGuard`'s `Drop` for the history of each step.
pub(in crate::native::jni) fn foreign_enter_idle(shared: &SharedVm) {
    // gci i4/fj: the census phases, as in `foreign_leave_idle`.
    let census = &shared.natives.jni_phase_census;
    let start = census.start();
    let mut clock = start;
    with_foreign_thread(|jt| {
        // gcd d3/k: withdraw the OS tid `foreign_leave_idle` published, while
        // still a counted mutator in Rust code: a pause waiting for us now
        // gets our cooperative arrival below, and an idle attachment has no
        // frame any pass could read. A no-op store when nothing was published.
        // gch h1/jw: the same predicate as the publish (`foreign_tid_publish_active`).
        if foreign_tid_publish_active() {
            shared.threads.thread_registry.clear_os_tid(jt.thread_id);
        }
        jt.tlab.retire();
        census.lap(JniPhase::ForeignTlabRetire, &mut clock);
        // gci i5/fj: what this deposit is made against, read BEFORE it, so a
        // pause that completes (a take-over) or folds in from here on makes
        // the next leave take the full wake. Only with the switch.
        let quiet_idle = foreign_quiet_idle_active();
        let sync = quiet_idle.then(|| NativeSync::read(shared, &jt.gc_block_state));
        crate::vm::NativeContextImpl { shared, thread: jt }.deposit_root_snapshot();
        set_foreign_idle_record(sync.map(|sync| ForeignIdleRecord {
            vm: shared.vm_identity,
            tid: jt.thread_id,
            sync,
            appended: 0,
        }));
        census.lap(JniPhase::ForeignDeposit, &mut clock);
        jt.gc_block_state
            .in_blocked_region
            .store(true, std::sync::atomic::Ordering::Release);
    });
    foreign_enter_idle_barrier(shared, clock, start);
}

/// The barrier half of [`foreign_enter_idle`] and
/// [`foreign_enter_idle_incremental`], after the thread's `in_blocked_region`
/// went up: count it blocked, and arrive if a pause raced in. `clock` and
/// `start` are the caller's census clocks.
pub(in crate::native::jni) fn foreign_enter_idle_barrier(
    shared: &SharedVm,
    mut clock: Option<std::time::Instant>,
    start: Option<std::time::Instant>,
) {
    let census = &shared.natives.jni_phase_census;
    let pre_stw = shared.mem.gc_barrier.mark_blocked_region_enter();
    if pre_stw {
        census.note(JniPhase::ForeignEnterRaced);
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
    census.lap(JniPhase::ForeignEnterBarrier, &mut clock);
    census.stop(JniPhase::ForeignEnter, start);
}

/// gci i5/fj (`CRATONVM_JNI_FOREIGN_QUIET_IDLE=1`): running -> idle WITHOUT
/// the full deposit, for a JNIEnv function that left idle quietly
/// ([`foreign_leave_idle`]), was entered through
/// [`ForeignJniEntry::enter_vm_only`] and ran no Java (the caller checked its
/// `LEAF_VM_ONLY` state). Answers `false`, having done nothing, unless the
/// snapshot the last full idle deposit published still describes the
/// attachment: the record is this attachment's, no pause completed and no
/// fold landed since it was read ([`NativeSync`]), no Java frame, and fewer
/// than [`INCREMENTAL_APPEND_LIMIT`] appends since that deposit; the caller
/// then goes idle the full way ([`foreign_enter_idle`]).
///
/// Why the snapshot plus the appended locals is the attachment's whole root
/// set (gcd d10/j's argument for [`native_call_reenter_incremental`], for the
/// idle attachment): such a function writes no `JvmThread` field on its
/// ordinary path (a pending exception or result goes through
/// `with_jni_context`, which escalates it), pushes no frame, stashes no deopt
/// frame, and creates objects only as locals of the innermost local frame --
/// the entries past `mark`, appended here, never replacing. A deleted local's
/// referent stays in the snapshot until the next full deposit (over-retention,
/// bounded by the append limit). The SATB buffer is flushed and the TLAB
/// retired as the full path does, and the flag goes up after the snapshot is
/// complete, exactly as there.
pub(in crate::native::jni) fn foreign_enter_idle_incremental(shared: &SharedVm, mark: LocalFramesMark) -> bool {
    let census = &shared.natives.jni_phase_census;
    let start = census.start();
    let mut clock = start;
    let done = with_foreign_thread(|jt| {
        let Some(record) = foreign_idle_record_for(shared.vm_identity, jt.thread_id) else {
            return false;
        };
        if !jt.frames.is_empty()
            || record.appended >= INCREMENTAL_APPEND_LIMIT
            || NativeSync::read(shared, &jt.gc_block_state) != record.sync
        {
            return false;
        }
        // As on the full path: withdraw the OS tid while still counted.
        if foreign_tid_publish_active() {
            shared.threads.thread_registry.clear_os_tid(jt.thread_id);
        }
        jt.tlab.retire();
        census.lap(JniPhase::ForeignTlabRetire, &mut clock);
        // `deposit_root_snapshot_inner`'s first step (fork6 GC_STRESS fix).
        shared.mem.heap.flush_thread_satb();
        let added = {
            let mut snapshot = jt.root_snapshot.lock();
            let before = snapshot.len();
            collect_local_ref_roots_since(mark, &mut snapshot);
            u32::try_from(snapshot.len() - before).unwrap_or(u32::MAX)
        };
        set_foreign_idle_record(Some(ForeignIdleRecord {
            appended: record.appended.saturating_add(added),
            ..record
        }));
        census.lap(JniPhase::ForeignDeposit, &mut clock);
        jt.gc_block_state
            .in_blocked_region
            .store(true, std::sync::atomic::Ordering::Release);
        true
    })
    .unwrap_or(false);
    if !done {
        return false;
    }
    census.note(JniPhase::ForeignEnterIncremental);
    foreign_enter_idle_barrier(shared, clock, start);
    true
}

// ---------------------------------------------------------------------------
// Per-function transition for an idle foreign thread
// (`CRATONVM_JNI_FOREIGN_TRANSITIONS`, default ON since 2026-10-10)
// ---------------------------------------------------------------------------
//
// gc-common w10-c, `docs/feature-designs/foreign-thread-attach.md`.
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
// It defaulted on (gc JNI retirement, 2026-10-10) after the
// `foreign_attach_soak` (`--cfg foreign_attach_soak`), the JNA native-thread
// callbacks of `GcJniLibrariesProbe.java` and `Gcd1JniRootsProbe`
// ran clean with it; `=0` is the kill switch.

/// `CRATONVM_JNI_FOREIGN_TRANSITIONS`, latched like every flag. Default ON
/// since the gc JNI retirement (2026-10-10); `=0` (or any off word) kills it.
///
/// gcd d10/j: resolved with the other two JNI switches as ONE package
/// ([`jni_switches`]): on with `CRATONVM_JNI_NATIVE_TRANSITIONS` unless set to
/// `0`, and NEVER on without indirect locals -- an attached thread whose
/// transitions run while its locals are raw addresses is the configuration
/// that read a decommitted span after a move (`jniroots_C_1`, rc 139,
/// `common-w2c-jni-local-refs-are-raw-addresses-FIXED-20261010.md`).
pub(in crate::native::jni) fn foreign_transitions_active() -> bool {
    if let Some(on) = foreign_transitions_test_override() {
        return on;
    }
    jni_switches().foreign_transitions
}

/// gcd d10/j: the three JNI in-native switches, resolved together once per
/// VM (gci i27/kd: it was once per process; [`jni_switches`]), so the
/// combinations the round measured are the only ones that can be selected.
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
/// ([`locals_are_indirect_here`]), so `native` needs no such rule.
///
/// gc JNI retirement (2026-10-10, `common-w2c` / `common-w9g`): all three
/// unset -- the default -- is arm D: indirect locals and the foreign
/// transitions ON, the native transitions OFF. Arm D costs the default
/// dispatch nothing measurable (the indirection is one thread-local slot read,
/// and the foreign entry is one thread-local check on a Java thread); the
/// package's cost is the native transitions' per-call deposit, which stays
/// opt-in. `CRATONVM_JNI_INDIRECT_LOCALS=0` is the kill switch of both (the
/// foreign transitions are refused without it), and
/// `CRATONVM_JNI_FOREIGN_TRANSITIONS=0` kills the foreign transitions alone.
///
/// gcf f1/j: `deposit_envelope_first` (`CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST`)
/// rides on the package too: on with the native transitions unless set off.
/// It only changes how the package's own in-native deposit walks compiled
/// frames ([`in_native_envelope_first_enabled`]), so it is meaningless without
/// them and is never on without them.
///
/// gcf f2/j: `deposit_repeat` (`CRATONVM_JNI_IN_NATIVE_REPEAT`) rides on the
/// package the same way ([`InNativeRepeat`]); `repeat_audit`
/// (`CRATONVM_DBG_JNI_REPEAT_AUDIT=1`) and `bulk_array_copies`
/// (`CRATONVM_JNI_BULK_ARRAY_COPY`, default ON since gcg g5/jn with `=0` the
/// kill switch, independent of the package; [`bulk_array_copies_from`]) are
/// latched here only so they cost no static of their own; the pure rule below
/// leaves both off and [`jni_switches`] sets them.
///
/// JIT round 16 wave 11 (lane jnieintr): `in_native_no_signal`
/// (`CRATONVM_JNI_IN_NATIVE_NO_SIGNAL`, a correctness fix: on with the
/// package, `=0` kills it) and `ffm_repeat` (`CRATONVM_FFM_IN_NATIVE_REPEAT=1`,
/// a performance switch, default off, independent of the package) are latched
/// here too, for the same reason; see [`in_native_no_signal_from`] and
/// [`repeat_armed_from`]. The pure rule leaves `ffm_repeat` off and
/// `in_native_no_signal` equal to `native_transitions`.
///
/// JIT round 16 wave 12 (lane inpkg): `in_native_doors`
/// (`CRATONVM_JNI_IN_NATIVE_DOORS`, a correctness fix: on with the package,
/// `=0` kills it) rides on the package the same way; see
/// [`in_native_doors_from`]. The pure rule leaves it equal to
/// `native_transitions`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct JniSwitches {
    pub(in crate::native::jni) indirect_locals: bool,
    pub(in crate::native::jni) foreign_transitions: bool,
    pub(in crate::native::jni) native_transitions: bool,
    pub(in crate::native::jni) deposit_envelope_first: bool,
    pub(in crate::native::jni) deposit_repeat: bool,
    pub(in crate::native::jni) blocking_repeat: bool,
    pub(in crate::native::jni) repeat_audit: bool,
    pub(in crate::native::jni) bulk_array_copies: bool,
    pub(in crate::native::jni) in_native_no_signal: bool,
    pub(in crate::native::jni) ffm_repeat: bool,
    pub(in crate::native::jni) in_native_doors: bool,
    /// gch h1/jw (`gcd-d2i-foreign-attached-threads-publish-no-os-tid`):
    /// `CRATONVM_JNI_FOREIGN_TID_PUBLISH`, default ON, `=0` the kill switch;
    /// see [`foreign_tid_publish_active`]. Not part of the package rule: the
    /// pure rule leaves it on and [`jni_switches`] reads the switch.
    pub(in crate::native::jni) foreign_tid_publish: bool,
    /// gch h2/jr (`gch-h1jw-proposal-in-native-arrival-without-waiting-20261008`):
    /// `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT=1`, default OFF, only with the
    /// package; see [`in_native_arrive_no_wait_from`]. The pure rule leaves it
    /// off and [`jni_switches`] reads the switch.
    pub(in crate::native::jni) in_native_arrive_no_wait: bool,
    /// gch h4/bp (`gcg-g4jn-proposal-in-native-band-scanned-at-the-pause-DONE-20261015`):
    /// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`, on with the package and its
    /// no-signal mark unless set off (gci i1/jn, the flip; `=0` kills it); see
    /// [`in_native_band_at_pause_from`]. The pure rule leaves it equal to
    /// `native_transitions` (the no-signal mark's own pure value) and
    /// [`jni_switches`] reads the switch. (Dev's interpreter round i5 GC
    /// triage flipped the same default independently: gch h4b soundness and
    /// cost C passed.)
    pub(in crate::native::jni) in_native_band_at_pause: bool,
    /// gci i5/fj (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`):
    /// `CRATONVM_JNI_FOREIGN_QUIET_IDLE`, default ON since the vi5-fj verdict
    /// (`=0` the kill switch; gci i14/jh: this said "default OFF"),
    /// independent of the package; see [`foreign_quiet_idle_active`]. The pure
    /// rule leaves it off and [`jni_switches`] reads the switch
    /// (`runtime_flag_default_on`).
    pub(in crate::native::jni) foreign_quiet_idle: bool,
    /// gci i20/jt (`gci-i19jt-jni-calls-serialise-on-the-barrier-lock-FIXED-20261017`
    /// item 1): `CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE=1`, default OFF; the
    /// in-native leave takes no barrier lock when no pause is in progress
    /// (`GcBarrier::leave_in_native_lockfree_if`, which falls back to the
    /// locked leave without the flag-first request order). Outside the
    /// package rule: the pure rule leaves it off and [`jni_switches`] reads
    /// the switch (`runtime_flag_on`).
    pub(in crate::native::jni) in_native_lockfree_leave: bool,
    /// gci i48 lane jl48 (i10al item 4, `ja11_w_tag`):
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_SNAPSHOT`, default ON, `=0` the kill
    /// switch: the in-native leave's full wake keeps the snapshot its window's
    /// pauses folded (`NativeContextImpl::check_post_block_gc_in_native`) and
    /// a vm-only function goes back incrementally after it
    /// ([`NativeLeave::KeptWake`]). Off, every such wake refreshes the snapshot
    /// as before. Outside the package rule: the pure rule leaves it on and
    /// [`jni_switches`] reads the switch (`runtime_flag_default_on`).
    pub(in crate::native::jni) in_native_keep_snapshot: bool,
    /// gcj j1/jn: `CRATONVM_JNI_IN_NATIVE_BAND_NARROW` (gci i28/nb, default
    /// ON since the gck flip, `=0` kills), latched here once per VM. It was a flag-snapshot lookup (two
    /// hash probes) on every banded JNI call ([`in_native_band_at_pause`]).
    /// Outside the package rule: the pure rule leaves it off and
    /// [`jni_switches`] reads the switch ([`in_native_band_narrow_enabled`]).
    pub(in crate::native::jni) in_native_band_narrow: bool,
    /// gcj j1/jn: `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT=1` (gci i24/bp,
    /// default OFF), latched here once per VM. It was a flag-snapshot lookup
    /// on every banded JNI call of a Generational heap
    /// ([`native_call_enter_native`]) and twice per Generational pause with a
    /// band switch on (`gc_and_alloc::in_native_credit_stamp`). Outside the
    /// package rule: the pure rule leaves it off and [`jni_switches`] reads the
    /// switch ([`in_native_proven_depth_credit_enabled`]).
    pub(in crate::native::jni) in_native_proven_depth_credit: bool,
    /// gcj j1/jn (`gcj-j1jn-foreign-attached-jni-allocation-never-collects`):
    /// `CRATONVM_JNI_FOREIGN_ALLOC_LADDER`, default ON: a refused JNI
    /// allocation of a foreign-attached thread may run the native ladder like
    /// any other thread's ([`jni_alloc_may_collect`]). Outside the package
    /// rule: the pure rule leaves it off and [`jni_switches`] reads the switch.
    pub(in crate::native::jni) foreign_alloc_ladder: bool,
    /// gcj j1/jn (`gcj-j1jn-jni-array-past-the-vm-limit-is-java-heap-space-20261019`):
    /// `CRATONVM_JNI_ARRAY_VM_LIMIT`, default ON, `=0` the kill switch: a JNI
    /// array request longer than HotSpot's limit is refused at once with
    /// `Requested array size exceeds VM limit` ([`jni_array_refused_for_its_length`]).
    /// Outside the package rule: the pure rule leaves it on and
    /// [`jni_switches`] reads the switch (`runtime_flag_default_on`).
    pub(in crate::native::jni) array_vm_limit: bool,
    /// gcj j2/jn2 (cut 1 of the transition ledger):
    /// `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`, default ON since 2026-10-26
    /// (WARNING t1-user-flip-20261026, `=0` kills): a JNI native
    /// dispatch resolves these switches once ([`switches_once_per_call_enabled`]).
    /// Outside the package rule: the pure rule leaves it off and
    /// [`jni_switches`] reads the switch.
    pub(in crate::native::jni) switches_once_per_call: bool,
    /// gcj j2/jn2 (cut 2 of the transition ledger):
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`, default OFF, Generational only:
    /// a JNI call going into native keeps its TLAB and a pause retires it on
    /// its behalf ([`in_native_keeps_tlab`]). Outside the package rule: the
    /// pure rule leaves it off and [`jni_switches`] reads the switch.
    pub(in crate::native::jni) in_native_keep_tlab: bool,
    /// t1 wave 8 (lane JNICUT, cut 0 continued):
    /// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1` (gci i5g/g1r, default ON,
    /// `=0` kills), latched here once per VM. It was a flag-snapshot lookup
    /// (`runtime_flag_default_on`) on every banded JNI call of a G1 heap
    /// ([`in_native_band_heap_admits`]), the one the j1/jn latch missed.
    /// Outside the package rule: the pure rule leaves it on.
    pub(in crate::native::jni) in_native_band_at_pause_g1: bool,
    /// gcj j2/jn2 (the up-call proposal's step 3): `CRATONVM_JNI_STATIC_EXACT_MID`
    /// (round 15 wave 7, default ON, `=0` kills), latched once per VM; it was
    /// a flag-snapshot lookup per `CallStatic*` whose `clazz` is not the
    /// method's declaring class ([`jni_call_static`]). Outside the package
    /// rule: the pure rule leaves it on.
    pub(in crate::native::jni) static_exact_mid: bool,
    /// gcj j2/jn2: `CRATONVM_JNI_NONVIRTUAL_EXACT_MID` (round 15 wave 6,
    /// default ON, `=0` kills), latched likewise ([`jni_call_nonvirtual`]).
    pub(in crate::native::jni) nonvirtual_exact_mid: bool,
    /// gcj j2/jn2: `CRATONVM_JNI_VIRTUAL_UPCALL_SELECTS=1`, default OFF
    /// ([`jni_virtual_upcall_selection`]). Outside the package rule: the pure
    /// rule leaves it off.
    pub(in crate::native::jni) virtual_upcall_selects: bool,
    /// gcj j2/jn2: `CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES`, default ON, `=0`
    /// kills ([`jni_new_string_hotspot_edges`]). Outside the package rule: the
    /// pure rule leaves it on.
    pub(in crate::native::jni) new_string_hotspot_edges: bool,
    /// JIT round 21 wave 19 (lane jnilazy, FF3-3): `CRATONVM_JNI_IN_NATIVE_LAZY`,
    /// on with the package and its doors unless set off; see
    /// [`in_native_lazy_from`] and [`JniNativeCall::enter_engaged`].
    pub(in crate::native::jni) in_native_lazy: bool,
}

/// JIT round 21 wave 19 (lane jnilazy, FF3-3;
/// `docs/feature-designs/native-dispatch-architecture.md`): may a
/// JNI native call that holds no local reference go into C through the LAZY
/// in-native state (`GcBlockState::enter_lazy_native`, the FFM downcall's
/// `_thread_in_native`) instead of the package's root deposit? Only with the
/// package (`native_transitions`: every JNIEnv function then leaves native
/// first) and its doors (`in_native_doors`: so does an FFM up-call and the
/// embedding API), and unless `CRATONVM_JNI_IN_NATIVE_LAZY` is set off.
pub(in crate::native::jni) fn in_native_lazy_from(armed: bool, explicit: Option<bool>) -> bool {
    armed && explicit != Some(false)
}

/// gch h4/bp: does a JNI native called from compiled code leave its compiled
/// band to the pause ([`in_native_band_at_pause`])? Only with the package
/// and its no-signal mark (so the thread whose C code runs on through a pause
/// is in no take-over or helper-window roster) -- which `jni_switches` passes
/// as `armed` -- and unless `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` is set off.
///
/// gci i1/jn (2026-10-11): default ON with the package (the gch handoff's
/// band B flip): cost C passed (`noop` 1175 vs 2448 ns, `dep_coverage` 1545
/// -> 46 ns, `docs/feature-designs/real-jdk-mode.md` addendum
/// B) and soundness passed after the leave-window fix (`vh4b-bp.list`: on
/// 5/5 SAME, the skip control DIFF 2/2). `=0` (or any off word) is the kill
/// switch. (gci i19/jt: since the package's own default flip of 2026-10-13
/// this is on the default dispatch of every JNI native called from compiled
/// code on Generational -- and on G1 with
/// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1`, default ON since the vi6-g1r
/// verdict; this said "the package itself stays opt-in".)
pub(in crate::native::jni) fn in_native_band_at_pause_from(armed: bool, explicit: Option<bool>) -> bool {
    armed && explicit != Some(false)
}

/// gch h2/jr: does a thread going into native for a JNI native call, when a
/// pause raced its entry, arrive at that pause WITHOUT parking
/// (`GcBarrier::arrive_in_native`) instead of `arrive_and_wait_auto`? Only
/// with the package (no thread is in native without it) and its no-signal
/// mark (`in_native_no_signal`, so the thread whose C code runs on through the
/// pause is in no take-over or helper-window roster) -- `jni_switches` passes
/// both as `armed` -- and only when `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT` is
/// set on: a barrier-protocol change awaiting its probe rows (`vh2-jr.list`,
/// block B).
pub(in crate::native::jni) fn in_native_arrive_no_wait_from(armed: bool, explicit: Option<bool>) -> bool {
    armed && explicit == Some(true)
}

/// gch h1/jw: `CRATONVM_JNI_FOREIGN_TID_PUBLISH`'s value rule: on unless set
/// off (`0` / `false` / `off` / `no`).
pub(in crate::native::jni) fn foreign_tid_publish_from(explicit: Option<bool>) -> bool {
    explicit != Some(false)
}

/// gch h1/jw (`gcd-d2i-foreign-attached-threads-publish-no-os-tid-FIXED-20261010`,
/// the defaults-path half): does a foreign-attached thread publish its OS tid
/// for as long as it runs Java or a JNI function ([`foreign_leave_idle`]) and
/// withdraw it when it goes idle again ([`foreign_enter_idle`])?
///
/// The publish and the withdrawal were gcd d3/k's, and rode on
/// `CRATONVM_JNI_FOREIGN_TRANSITIONS` only because they were written with it.
/// With the defaults an attached thread still becomes a counted mutator for
/// every Java call it makes (`ForeignCallGuard`, through
/// `foreign_leave_idle`), and ran that call -- interpreted and compiled code --
/// with `os_tid == 0`: in no take-over roster (a pause waited for its next
/// poll instead of freezing it), in no helper-window roster when it blocked
/// inside the call, and a `ROSTER HOLE` under `CRATONVM_XT_ROOT_SCAN_AUDIT=1`.
/// None of that depends on the JNI functions' transitions, which stay with the
/// package. While idle the tid is withdrawn exactly as under the package, so
/// no pass signals or suspends a host thread parked in its own event loop.
///
/// `CRATONVM_JNI_FOREIGN_TID_PUBLISH=0` restores the pre-gch rule (published
/// only with the foreign transitions).
pub(in crate::native::jni) fn foreign_tid_publish_active() -> bool {
    foreign_transitions_active()
        || foreign_tid_publish_test_override().unwrap_or_else(|| jni_switches().foreign_tid_publish)
}

/// gci i5/fj (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`, the
/// cut vi4-fj A named: the no-churn Generational arm read PROTOCOL, PR/T
/// 0.53): `CRATONVM_JNI_FOREIGN_QUIET_IDLE`, default ON since the gci
/// orchestrator's vi5-fj verdict (2026-10-13); `=0` (or any off word) is the
/// kill switch, read with `runtime_flag_default_on` in [`jni_switches`]
/// (gci i14/jh: this said "`=1`, default OFF").
///
/// An idle foreign attachment paid, per JNIEnv function (and per outermost
/// Java call), a FULL wake on the way out of idle -- `check_post_block_gc`,
/// whose snapshot refresh is a second full deposit -- and a full deposit on
/// the way back in, although nothing it holds had moved. With the switch:
///
/// * a QUIET LEAVE ([`foreign_leave_idle`]): when no pause completed and no
///   relocation was folded into the thread since its last idle deposit
///   ([`ForeignIdleRecord`], compared under the barrier lock by
///   `GcBarrier::leave_blocked_region_flagged_if`) and the attachment holds
///   no Java frame, the flag drops and the wake is skipped: the fixup is
///   empty, there is no frame to remap or write back, and the snapshot the
///   idle deposit published is what the refresh would publish again. The
///   gcd d5/f leave of a JNI native in native, for the idle attachment;
/// * an INCREMENTAL RE-ENTRY ([`foreign_enter_idle_incremental`]) after a
///   function entered through [`ForeignJniEntry::enter_vm_only`] (or
///   `enter_leaf`) that left quietly, ran no Java (`with_jni_context`
///   escalates it, [`LEAF_VM_ONLY_ESCALATED`]) and saw no pause complete:
///   the locals it minted are appended to the snapshot and the SATB buffer
///   flushed, without the full deposit -- gcd d10/j's
///   [`native_call_reenter_incremental`], for the idle attachment. At most
///   [`INCREMENTAL_APPEND_LIMIT`] appends between two full deposits (a deleted
///   local's referent is over-retained until then, never lost).
///
/// Every other transition is the full one, as with the switch off. Inert
/// for every thread that is not foreign-attached.
pub(in crate::native::jni) fn foreign_quiet_idle_active() -> bool {
    foreign_quiet_idle_test_override().unwrap_or_else(|| jni_switches().foreign_quiet_idle)
}

/// gci i5/fj: what the last FULL idle deposit of this OS thread's foreign
/// attachment ([`foreign_enter_idle`]) was made against, kept in
/// [`NativeCallTls::foreign_idle`] while [`foreign_quiet_idle_active`].
/// It answers for one attachment only: the VM and the registry id must match
/// (a detach forgets it, [`forget_foreign_idle_record`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::native::jni) struct ForeignIdleRecord {
    /// `SharedVm::vm_identity` of the attachment's VM.
    pub(in crate::native::jni) vm: usize,
    /// The attachment's registry id.
    pub(in crate::native::jni) tid: ThreadId,
    /// [`NativeSync`] read just before that deposit: a pause that completes,
    /// or a fold into the thread, after the read makes it differ.
    pub(in crate::native::jni) sync: NativeSync,
    /// Snapshot entries incremental re-entries appended since that deposit.
    pub(in crate::native::jni) appended: u32,
}

/// gci i5/fj: this OS thread's [`ForeignIdleRecord`] when it belongs to
/// `vm` / `tid`.
pub(in crate::native::jni) fn foreign_idle_record_for(vm: usize, tid: ThreadId) -> Option<ForeignIdleRecord> {
    JNI_NATIVE_CALL
        .try_with(|t| t.foreign_idle.get())
        .ok()
        .flatten()
        .filter(|r| r.vm == vm && r.tid == tid)
}

/// gci i5/fj: store (or, `None`, forget) this OS thread's [`ForeignIdleRecord`].
pub(in crate::native::jni) fn set_foreign_idle_record(record: Option<ForeignIdleRecord>) {
    let _ = JNI_NATIVE_CALL.try_with(|t| t.foreign_idle.set(record));
}

/// gci i5/fj: forget the record (detach; a full wake). Callable during TLS
/// teardown.
pub(in crate::native::jni) fn forget_foreign_idle_record() {
    set_foreign_idle_record(None);
}

/// JIT round 16 wave 12 (lane inpkg, the door audit of
/// `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-20261002`):
/// do the three doors from C back into the VM that bypassed the JNIEnv table
/// leave native first? On with the package unless
/// `CRATONVM_JNI_IN_NATIVE_DOORS` is set off; meaningless without it.
///
/// * an FFM up-call stub that a JNI native's C code calls: the up-call ran Java
///   on a thread still in native (`ffm_upcall_attach` ->
///   [`ForeignJniEntry::enter_for_upcall`]), and a JNI native called inside an
///   FFM downcall (whose context the stub's entry finds first) is kept
///   counted ([`JniNativeCall`]'s `enter_engaged`);
/// * the embedding library's flat `cratonvm_*` API, called from C inside a
///   JNI native ([`enter_vm_for_embedding_call`]).
pub(in crate::native::jni) fn in_native_doors_from(native_transitions: bool, explicit: Option<bool>) -> bool {
    native_transitions && explicit != Some(false)
}

/// The package's default when `CRATONVM_JNI_NATIVE_TRANSITIONS` is unset.
///
/// JIT round 16 wave 12 (lane inpkg) staged the flip here; the user applied
/// it on 2026-10-13 (WARNING gci-user-flip-20261013, `gcd-d2i`): the body is
/// `explicit_default != Some(false)`, so the package is ON unless
/// `CRATONVM_JNI_NATIVE_TRANSITIONS_DEFAULT=0` or
/// `CRATONVM_JNI_NATIVE_TRANSITIONS=0` (both kill switches). An explicit
/// `CRATONVM_JNI_NATIVE_TRANSITIONS` always wins over this default. (gci
/// i19/jt: this said "Off unless `..._DEFAULT=1`".)
pub(in crate::native::jni) fn native_transitions_default_from(explicit_default: Option<bool>) -> bool {
    // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage
    // (gcd-d2i): the one-line flip named above.
    explicit_default != Some(false)
}

/// `CRATONVM_JNI_NATIVE_TRANSITIONS`' value as the package rule takes it:
/// the explicit setting, or `Some(true)` when unset and the package default
/// ([`native_transitions_default_from`]) is on.
pub(in crate::native::jni) fn native_transitions_explicit_or_default(
    explicit: Option<bool>,
    explicit_default: Option<bool>,
) -> Option<bool> {
    explicit.or_else(|| native_transitions_default_from(explicit_default).then_some(true))
}

/// JIT round 16 wave 11 (lane jnieintr): does the package's in-native bracket
/// raise `GcBlockState::foreign_native`, so that no pause signals the thread
/// while its C code runs (`thread_registry::ffm_in_native_signal_exempt`)?
/// On with the package unless `CRATONVM_JNI_IN_NATIVE_NO_SIGNAL` is set off;
/// meaningless without the package (no thread is ever in native then).
pub(in crate::native::jni) fn in_native_no_signal_from(native_transitions: bool, explicit: Option<bool>) -> bool {
    native_transitions && explicit != Some(false)
}

/// JIT round 16 wave 11 (lane jnieintr, FFM16-3): is the in-native repeat
/// ([`InNativeRepeat`]) armed for an in-native deposit? With the package's
/// own repeat (`deposit_repeat`), or with `CRATONVM_FFM_IN_NATIVE_REPEAT=1`
/// without the package -- the quiet GC-safe FFM downcall's deposit
/// (`NativeContextImpl::begin_native_region_quiet`) runs under the same
/// frozen-band memo, and the repeat's key names nothing JNI-specific. Only a
/// flag-raising deposit inside that memo ever reaches the check.
pub(in crate::native::jni) fn repeat_armed_from(switches: &JniSwitches, raise_blocked_flag: bool) -> bool {
    raise_blocked_flag && (switches.deposit_repeat || switches.ffm_repeat || switches.blocking_repeat)
}

/// The package rule of [`JniSwitches`], pure: each argument is a flag's
/// explicit value ([`jni_switch_value`]), `None` when it is unset.
pub(in crate::native::jni) fn jni_switches_from(
    native: Option<bool>,
    indirect: Option<bool>,
    foreign: Option<bool>,
    envelope_first: Option<bool>,
    repeat: Option<bool>,
) -> JniSwitches {
    let native_transitions = native.unwrap_or(false);
    let indirect_locals = indirect.unwrap_or(true);
    let foreign_transitions = foreign.unwrap_or(true) && indirect_locals;
    let deposit_envelope_first = native_transitions && envelope_first.unwrap_or(true);
    let blocking_repeat = repeat.unwrap_or(true);
    let deposit_repeat = native_transitions && blocking_repeat;
    JniSwitches {
        indirect_locals,
        foreign_transitions,
        native_transitions,
        deposit_envelope_first,
        deposit_repeat,
        blocking_repeat,
        repeat_audit: false,
        bulk_array_copies: false,
        in_native_no_signal: native_transitions,
        ffm_repeat: false,
        in_native_doors: native_transitions,
        foreign_tid_publish: true,
        in_native_arrive_no_wait: false,
        // gci i1/jn: default ON with the package (and its no-signal mark,
        // `native_transitions` in the pure rule).
        in_native_band_at_pause: native_transitions,
        // gci i5/fj: opt-in, outside the package rule.
        foreign_quiet_idle: false,
        // gci i20/jt: opt-in, outside the package rule.
        in_native_lockfree_leave: false,
        // gci i48 jl48: on unless killed, outside the package rule.
        in_native_keep_snapshot: true,
        // gcj j1/jn: the three opt-ins latched per VM, outside the package rule.
        in_native_band_narrow: false,
        in_native_proven_depth_credit: false,
        foreign_alloc_ladder: false,
        // gcj j1/jn: on unless killed, outside the package rule.
        array_vm_limit: true,
        // gcj j2/jn2: the two opt-in cuts of the transition ledger.
        switches_once_per_call: false,
        in_native_keep_tlab: false,
        // t1 w8 JNICUT: on unless killed, outside the package rule.
        in_native_band_at_pause_g1: true,
        // gcj j2/jn2: the up-call switches, outside the package rule.
        static_exact_mid: true,
        nonvirtual_exact_mid: true,
        virtual_upcall_selects: false,
        new_string_hotspot_edges: true,
        // JIT round 21 wave 19 (lane jnilazy): on with the package; the doors
        // are on with it in the pure rule.
        in_native_lazy: native_transitions,
    }
}

/// A JNI switch's explicit value: `None` when the variable is unset or empty,
/// otherwise [`indirect_locals_from`]'s rule (an off word is `Some(false)`).
pub(in crate::native::jni) fn jni_switch_value(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        None | Some("") => None,
        set => Some(indirect_locals_from(set)),
    }
}

/// The [`JniSwitches`] of the VM this OS thread is acting for.
///
/// gci i27/kd (`gch-h2jr-review-findings-RETIRED-RULING-20261018` item 9): they were one
/// process `OnceLock`, so the first reader's answer held for every VM of the
/// process, and a VM built under a per-thread flag override could not get its
/// own package. They are read once per VM now ([`SharedVm::jni_switches`],
/// [`JniSwitches::from_flags`] at construction) and resolved here per call:
/// the thread's JNI context (installed for every JNI native dispatch, so the
/// dispatch path pays one thread-local borrow and a copy), else the VM of the
/// thread's foreign attachment ([`FOREIGN_ATTACH_VM`]), else -- a thread
/// acting for no recorded VM: a collector or tool path without a VM in hand,
/// a bare test fixture -- the flag snapshot, read once per OS thread
/// (`NativeCallTls::switches_memo`). Callers holding the VM use
/// [`jni_switches_of`]. No `process_vm()` guess (it answers the
/// LAST VM built, and its temporary `Arc` could run a VM's teardown here).
/// Every VM of a process reads the same snapshot unless it was built under a
/// per-thread override, so the answers are the old latch's on every
/// production path.
pub(in crate::native::jni) fn jni_switches() -> JniSwitches {
    // JIT round 21 wave 20 (lane jniharness, JNI6-1): a unit test's per-thread
    // package ([`NativeCallTls::switches_override`]); the switches below are
    // latched per VM at its construction, so `with_thread_overrides` cannot
    // reach a running VM's.
    #[cfg(test)]
    {
        if let Some(switches) = JNI_NATIVE_CALL
            .try_with(|t| t.switches_override.get())
            .ok()
            .flatten()
        {
            return switches;
        }
    }
    let in_context = JNI_SHARED_VM
        .try_with(|c| {
            c.try_borrow()
                .ok()
                .and_then(|vm| vm.as_ref().map(|vm| vm.jni_switches))
        })
        .ok()
        .flatten();
    if let Some(switches) = in_context {
        return switches;
    }
    let attached = FOREIGN_ATTACH_VM
        .try_with(|c| {
            c.try_borrow()
                .ok()
                .and_then(|w| w.as_ref().and_then(Weak::upgrade))
                .map(|vm| vm.jni_switches)
        })
        .ok()
        .flatten();
    if let Some(switches) = attached {
        return switches;
    }
    JNI_NATIVE_CALL
        .try_with(|t| match t.switches_memo.get() {
            Some(switches) => switches,
            None => {
                let switches = JniSwitches::read_flags(false);
                t.switches_memo.set(Some(switches));
                switches
            }
        })
        .unwrap_or_else(|_| JniSwitches::read_flags(false))
}

/// [`jni_switches`] for a caller that holds the VM.
pub(in crate::native::jni) fn jni_switches_of(shared: &SharedVm) -> JniSwitches {
    shared.jni_switches
}

/// gci i48 lane jl48: `CRATONVM_JNI_IN_NATIVE_KEEP_SNAPSHOT` of `shared`
/// (read once per VM, [`JniSwitches::in_native_keep_snapshot`]): may the
/// in-native leave's full wake keep the folded snapshot? Read by
/// `NativeContextImpl::check_post_block_gc_in_native`.
pub(crate) fn in_native_keep_snapshot_of(shared: &SharedVm) -> bool {
    jni_switches_of(shared).in_native_keep_snapshot
}

impl JniSwitches {
    /// gci i27/kd: the switches of a VM being built, read once from the flag
    /// snapshot (`SharedVm` construction); a refused
    /// `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` is reported on stderr once per VM.
    pub(crate) fn from_flags() -> Self {
        Self::read_flags(true)
    }

    /// The package rule over the flag snapshot; `report` prints the refusal
    /// line (only [`JniSwitches::from_flags`] does).
    pub(in crate::native::jni) fn read_flags(report: bool) -> Self {
        let read =
            |name: &str| jni_switch_value(cratonvm_types::flags::runtime_var(name).ok().as_deref());
        let foreign = read("CRATONVM_JNI_FOREIGN_TRANSITIONS");
        // JIT round 16 wave 12 (lane inpkg): the staged flip, see
        // `native_transitions_default_from`.
        let native = native_transitions_explicit_or_default(
            read("CRATONVM_JNI_NATIVE_TRANSITIONS"),
            read("CRATONVM_JNI_NATIVE_TRANSITIONS_DEFAULT"),
        );
        let mut switches = jni_switches_from(
            native,
            read("CRATONVM_JNI_INDIRECT_LOCALS"),
            foreign,
            read("CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST"),
            read("CRATONVM_JNI_IN_NATIVE_REPEAT"),
        );
        // JIT round 16 wave 11 (lane jnieintr): see `in_native_no_signal_from`
        // and `repeat_armed_from`.
        switches.in_native_no_signal = in_native_no_signal_from(
            switches.native_transitions,
            read("CRATONVM_JNI_IN_NATIVE_NO_SIGNAL"),
        );
        switches.ffm_repeat = read("CRATONVM_FFM_IN_NATIVE_REPEAT") == Some(true);
        // JIT round 16 wave 12 (lane inpkg): see `in_native_doors_from`.
        switches.in_native_doors = in_native_doors_from(
            switches.native_transitions,
            read("CRATONVM_JNI_IN_NATIVE_DOORS"),
        );
        // gcf f2/j: the two that are not part of the package rule.
        switches.repeat_audit = (switches.deposit_repeat || switches.ffm_repeat || switches.blocking_repeat)
            && read("CRATONVM_DBG_JNI_REPEAT_AUDIT") == Some(true);
        switches.bulk_array_copies = bulk_array_copies_from(read("CRATONVM_JNI_BULK_ARRAY_COPY"));
        // gch h1/jw: see `foreign_tid_publish_active`.
        switches.foreign_tid_publish =
            foreign_tid_publish_from(read("CRATONVM_JNI_FOREIGN_TID_PUBLISH"));
        // gch h2/jr: see `in_native_arrive_no_wait_from`. Only with the
        // no-signal mark too: a thread whose C code runs through a pause must
        // be out of the take-over and helper-window rosters
        // (`thread_registry::ffm_in_native_signal_exempt`), as an excluded
        // in-native thread is under the package's default.
        switches.in_native_arrive_no_wait = in_native_arrive_no_wait_from(
            switches.native_transitions && switches.in_native_no_signal,
            read("CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT"),
        );
        // gch h4/bp: see `in_native_band_at_pause_from`.
        switches.in_native_band_at_pause = in_native_band_at_pause_from(
            switches.native_transitions && switches.in_native_no_signal,
            read("CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE"),
        );
        // gci i5/fj: see `foreign_quiet_idle_active`. Default ON since the gci
        // orchestrator's vi5-fj verdict (2026-10-13: Gen no-churn U 0.745x the
        // old path, soundness rows clean on Gen/G1/ZGC); `=0` kills it.
        switches.foreign_quiet_idle =
            true;
        // gci i20/jt: see `JniSwitches::in_native_lockfree_leave`. Meaningful
        // only with the package (no thread is in native without it).
        switches.in_native_lockfree_leave = switches.native_transitions;
        // gci i48 jl48: see `JniSwitches::in_native_keep_snapshot`.
        switches.in_native_keep_snapshot =
            true;
        // gcj j1/jn: read once per VM here instead of once per banded call
        // (`JniSwitches::in_native_band_narrow` / `::in_native_proven_depth_credit`).
        // WARNING gck-jn2-user-flip-20261025: the narrowing is default ON
        // (`=0` kills, see `in_native_band_narrow_enabled`); the credit stays opt-in.
        switches.in_native_band_narrow = in_native_band_narrow_enabled();
        switches.in_native_proven_depth_credit = in_native_proven_depth_credit_enabled();
        // gcj j1/jn: see `foreign_alloc_ladder_enabled`.
        switches.foreign_alloc_ladder = foreign_alloc_ladder_enabled();
        // gcj j1/jn: see `jni_array_refused_for_its_length`.
        switches.array_vm_limit =
            true;
        // gcj j2/jn2: cut 1 (`switches_once_per_call_enabled`) and cut 2
        // (`in_native_keep_tlab_enabled`, meaningful only with the package:
        // no thread is in native without it).
        switches.switches_once_per_call = switches_once_per_call_enabled();
        switches.in_native_keep_tlab =
            switches.native_transitions && in_native_keep_tlab_enabled();
        // t1 w8 JNICUT: see `JniSwitches::in_native_band_at_pause_g1`.
        switches.in_native_band_at_pause_g1 = in_native_band_at_pause_g1_enabled();
        // gcj j2/jn2: the up-call switches, read once per VM with their old
        // readers' value rules (`jni_call_static`, `jni_call_nonvirtual`,
        // `jni_virtual_upcall_selection`).
        switches.static_exact_mid =
            true;
        switches.nonvirtual_exact_mid =
            true;
        // WARNING flip (gcj j3 close, 2026-10-20): default ON (`=0` kills); its use
        // stays Generational only (`jni_virtual_upcall_selection`). Rows on j3a:
        // block U ON SAME where the default DIFFed, regression rows jk15 / mf12 /
        // cr10 SAME on both arms, 200k-call reps no slower.
        switches.virtual_upcall_selects =
            true;
        switches.new_string_hotspot_edges =
            true;
        // JIT round 21 wave 19 (lane jnilazy, FF3-3): see `in_native_lazy_from`.
        // The pause claims a lazy thread only under the FFM switch
        // (`gc_and_alloc::ffm_in_native_lazy_claims_enabled`); with it killed a
        // lazy JNI call would hold every pause for its whole C call, so it
        // takes the deposit instead.
        switches.in_native_lazy = in_native_lazy_from(
            switches.native_transitions
                && switches.in_native_doors
                && read("CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY") != Some(false),
            read("CRATONVM_JNI_IN_NATIVE_LAZY"),
        );
        if report && foreign == Some(true) && !switches.foreign_transitions {
            eprintln!(
                "{}",
                concat!(
                    "[cratonvm] CRATONVM_JNI_FOREIGN_TRANSITIONS is ignored with ",
                    "CRATONVM_JNI_INDIRECT_LOCALS off: an attached thread's raw local refs ",
                    "would go stale across a moving collection. Unset ",
                    "CRATONVM_JNI_INDIRECT_LOCALS to keep both on."
                )
            );
        }
        switches
    }
}

/// gcf f1/j (`CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST`, on with the package,
/// `=0` kills it): may the package's in-native deposit skip, before any of the
/// band claims, a compiled-frame word that lies outside the heap's address
/// envelope (`VmHeap::conservative_addr_span`), and a whole frame whose band
/// holds no such word? Read by `jit::conservative_roots` only while
/// [`crate::jit::conservative_roots::in_native_deposit_armed`] holds, i.e.
/// inside [`native_call_enter_native`]'s deposit; every other scan is
/// unchanged. One latched load.
///
/// Exact by construction: a word outside the envelope is never an object
/// start, so it is neither a root, a pin, a G1 reject-pin nor a publication,
/// whatever the claims say about it; only the claims' diagnostic exclusion
/// counters stop counting such words. See
/// `docs/feature-designs/real-jdk-mode.md`.
pub(crate) fn in_native_envelope_first_enabled() -> bool {
    jni_switches().deposit_envelope_first
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
    pub(in crate::native::jni) transition: Option<Arc<SharedVm>>,
    /// `Some` iff this entry took an in-native JNI native call out of native
    /// ([`JniNativeCall`]) and must put it back when the function returns.
    ///
    /// gci i20/jt (`gci-i19jt-jni-calls-serialise-on-the-barrier-lock-FIXED-20261017`
    /// item 2): the VM by address, not a clone of the record's `Arc` -- two
    /// RMWs per non-leaf JNIEnv function in native on the one cache line
    /// every JNI thread of the VM writes. Valid for the entry's whole life:
    /// the entry is made and dropped inside one JNIEnv function called from
    /// the C code of the call whose [`NativeCallRecord`] owns an `Arc` of the
    /// same VM (and whose outermost dispatch's context guard owns another);
    /// a nested call through an up-call saves and restores that record, and
    /// `DetachCurrentThread` from a native in native is refused (gcd d3/k).
    pub(in crate::native::jni) native: Option<(*const SharedVm, *mut JvmThread)>,
    /// gcd d5/f: this entry opened a JNI leaf window
    /// ([`ForeignJniEntry::enter_leaf`]) and must close it -- or, if the
    /// function escalated to the full transition, put the call back in native.
    pub(in crate::native::jni) leaf: bool,
    /// gcd d5/f: this entry took the thread out of a GC-safe FFM downcall's
    /// region (`CRATONVM_FFM_DOWNCALL_GC_SAFE`: C code inside the downcall
    /// called a JNIEnv function) and must put it back.
    pub(in crate::native::jni) downcall: bool,
    /// gcd d10/j: this entry took an in-native call out of native QUIETLY for
    /// a function that runs no Java ([`ForeignJniEntry::enter_vm_only`]) and
    /// owns this thread's [`LEAF_VM_ONLY`] state: its drop may put the call
    /// back in native incrementally ([`native_call_reenter_incremental`]).
    pub(in crate::native::jni) vm_only: bool,
    /// gcd d10/j: with `vm_only`, the local frames' extent when the call left
    /// native: the locals past it are what the function minted.
    pub(in crate::native::jni) locals_mark: LocalFramesMark,
    /// gci i4/fj: with `transition` and the JNI phase census on, when the
    /// leave finished (`JniPhase::ForeignBody` is charged on drop).
    pub(in crate::native::jni) foreign_body: Option<std::time::Instant>,
    /// gci i5/fj (`CRATONVM_JNI_FOREIGN_QUIET_IDLE`): with `transition`, this
    /// entry left idle QUIETLY for a function that runs no Java
    /// ([`Self::enter_vm_only`]) and owns this thread's [`LEAF_VM_ONLY`]
    /// state: its drop may go idle incrementally
    /// ([`foreign_enter_idle_incremental`]); `locals_mark` is the local
    /// frames' extent after the leave.
    pub(in crate::native::jni) foreign_quiet: bool,
}

impl ForeignJniEntry {
    /// An entry that does nothing on drop.
    #[inline]
    pub(in crate::native::jni) fn inert() -> Self {
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
            foreign_body: None,
            foreign_quiet: false,
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
        Self::enter_outside_native(false)
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
        Self::enter_outside_native(true)
    }

    /// [`Self::enter`] past the in-native check: the FFM downcall region, then
    /// the idle foreign attachment. gci i5/fj: `vm_only` (from
    /// [`Self::enter_vm_only`]) lets the idle attachment go idle again
    /// incrementally ([`Self::enter_foreign`]).
    #[inline]
    pub(in crate::native::jni) fn enter_outside_native(vm_only: bool) -> Self {
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
        // The allocation ladder also needs an entry when the old foreign
        // transition diagnostic is off (including its raw-local fallback).
        // Its always-open local frames and published pins now make that safe.
        if !is_foreign_attached()
            || (!foreign_transitions_active() && !jni_switches().foreign_alloc_ladder)
        {
            return Self::inert();
        }
        Self::enter_foreign(vm_only)
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
            if let Some(entry) = Self::enter_leaf_window(handle, true) {
                return entry;
            }
        }
        Self::enter_vm_only()
    }

    /// gci i47w lane oz47 (`vi11-ja.list` `ja11_w_tag` past 600 s on i48j-dbg:
    /// the i48j native stacks show no wait, the main thread re-depositing its
    /// roots on every JNIEnv call of the agent's `GetObjectArrayElement` /
    /// `SetTag` / `DeleteLocalRef` loop while a daemon loops `System.gc()`):
    /// [`Self::enter_leaf`] for `DeleteLocalRef`, which only clears a slot of
    /// this thread's own local-handle table and reads no object. The leaf
    /// window opens even when a pause completed or folded into this thread
    /// since the deposit: the table may still name pre-move addresses, but
    /// nothing here resolves one -- the slot is zeroed by index, and the
    /// leave's remap of the table (`update_local_refs_after_gc`, by address)
    /// skips zeroed slots. The window still excludes a pause while the table
    /// is written. Any other handle shape takes [`Self::enter_leaf`]'s path.
    #[inline]
    pub(crate) fn enter_leaf_table_only(handle: JObject) -> Self {
        if native_transitions_active() {
            if let Some(entry) = Self::enter_leaf_window(handle, false) {
                return entry;
            }
        }
        Self::enter_vm_only()
    }

    /// `needs_current_table`: the function resolves `handle` (every leaf but
    /// `DeleteLocalRef`), so a relocation since the deposit refuses the
    /// window.
    #[inline(never)]
    pub(in crate::native::jni) fn enter_leaf_window(handle: JObject, needs_current_table: bool) -> Option<Self> {
        // gci i16/sn: a suspended thread stops before its leaf window too, as
        // HotSpot's leaf functions transition as every JNIEnv function does.
        hold_in_native_while_suspended();
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
                // JIT round 21 wave 19 (lane jnilazy): never over a LAZY
                // in-native call: a leaf window proves only that no pause
                // collects meanwhile, which says nothing about a pause that
                // claimed this thread and is scanning it now.
                let Some(rec) = slot.as_ref().filter(|r| r.in_native && r.lazy.is_none()) else {
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
                if needs_current_table
                    && NativeSync::read(&rec.shared, unsafe { &*rec.block_state }) != rec.sync
                {
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
    pub(in crate::native::jni) fn enter_from_native(vm_only: bool) -> Option<Self> {
        // gci i16/sn: a thread a debugger or an agent holds suspended stops
        // here, still in native (before the record leaves it), until resumed.
        hold_in_native_while_suspended();
        let (shared, thread, sync, lazy) = JNI_NATIVE_CALL
            .try_with(|t| {
                let mut slot = t.record.try_borrow_mut().ok()?;
                let rec = slot.as_mut().filter(|r| r.in_native)?;
                rec.in_native = false;
                // gci i20/jt: by address (see `ForeignJniEntry::native`).
                Some((Arc::as_ptr(&rec.shared), rec.thread, rec.sync, rec.lazy.is_some()))
            })
            .ok()
            .flatten()?;
        // SAFETY: the record that holds an `Arc` of this VM outlives this
        // entry (see `ForeignJniEntry::native`).
        let shared_vm: &SharedVm = unsafe { &*shared };
        if lazy {
            // JIT round 21 wave 19 (lane jnilazy, FF3-3): out of the lazy
            // state: one compare-exchange, waiting out a pause that claimed
            // the thread; then a counted mutator. No quiet / incremental
            // bookkeeping (nothing was deposited), so no `vm_only` state: the
            // drop goes back lazily, or the full way.
            // SAFETY: as below.
            native_call_leave_lazy(shared_vm, unsafe { &mut *thread });
            let mut entry = Self::inert();
            entry.native = Some((shared, thread));
            return Some(entry);
        }
        // SAFETY: `thread` is the `JvmThread` the dispatch that installed the
        // record runs on; it outlives the record (`JniNativeCall`'s drop
        // removes it before the dispatch returns), and no other `&mut` to it
        // is used while this one lives: the native is in C, and this function
        // body has not started.
        let leave = native_call_leave_native(shared_vm, unsafe { &mut *thread }, sync, vm_only);
        // gci i48 lane jl48: a kept wake leaves the snapshot and the rebased
        // `slot_origins` exact for the thread's unchanged frames, as a quiet
        // leave leaves the deposit's: the drop may go back incrementally.
        let kept = leave == NativeLeave::KeptWake;
        let quiet = leave == NativeLeave::Quiet || kept;
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
            unsafe { &*thread }
                .gc_block_state
                .slot_origins
                .lock()
                .clear();
            // gch h4/bp: and the band with them (the full re-entry records
            // the call's band again).
            // SAFETY: as above.
            clear_in_native_band(&unsafe { &*thread }.gc_block_state);
        }
        if kept && owns_vm_only {
            // gci i48 lane jl48: the kept snapshot describes the thread as of
            // this wake, so the drop's incremental test (`fresh == rec.sync`:
            // no pause completed and nothing was folded since) starts here.
            // Read with the flag down: no pause completes without this
            // thread from the wake on, so nothing slips in between.
            // SAFETY: as above; atomics of the thread's block state.
            let now = NativeSync::read(shared_vm, &unsafe { &*thread }.gc_block_state);
            let _ = JNI_NATIVE_CALL.try_with(|t| {
                if let Ok(mut slot) = t.record.try_borrow_mut() {
                    if let Some(rec) = slot.as_mut().filter(|r| r.thread == thread) {
                        rec.sync = now;
                    }
                }
            });
        }
        let mut entry = Self::inert();
        entry.native = Some((shared, thread));
        entry.vm_only = owns_vm_only;
        if owns_vm_only {
            entry.locals_mark = local_frames_mark();
        }
        Some(entry)
    }

    /// JIT round 16 wave 12 (lane inpkg): native -> VM for an FFM up-call
    /// that C code inside a JNI native in native makes into `vm`'s Java
    /// (`ffm_upcall_attach`), as HotSpot's up-call stub does
    /// (`ThreadInVMfromNative`): the up-call's Java runs counted, and the
    /// entry's drop puts the call back in native with a full deposit. Inert
    /// unless the innermost record of this thread is in native for `vm` (a
    /// stub of another VM runs on a helper thread while this one waits in C,
    /// GC-safe), and without [`in_native_doors_active`].
    pub(crate) fn enter_for_upcall(vm: usize) -> Self {
        if !in_native_doors_active() {
            return Self::inert();
        }
        let same_vm = JNI_NATIVE_CALL
            .try_with(|t| {
                t.record.try_borrow().is_ok_and(|slot| {
                    slot.as_ref()
                        .is_some_and(|r| r.in_native && r.shared.vm_identity == vm)
                })
            })
            .unwrap_or(false);
        if !same_vm {
            return Self::inert();
        }
        Self::enter_from_native(false).unwrap_or_else(Self::inert)
    }

    #[inline(never)]
    pub(in crate::native::jni) fn enter_foreign(vm_only: bool) -> Self {
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
        // gci i17/fz (i16/sn's foreign residual): an attached host thread a
        // debugger or an agent holds suspended stops here, still idle
        // (GC-blocked), before its leave -- HotSpot's `ThreadInVMfromNative`
        // holds an `AttachCurrentThread` thread at its next JNIEnv call too.
        hold_foreign_idle_while_suspended(&shared);
        let quiet = foreign_leave_idle(&shared);
        let mut entry = Self::inert();
        // gci i5/fj: a quiet leave of a function that runs no Java claims this
        // thread's `LEAF_VM_ONLY` state -- only when no other entry holds it
        // (at depth 0 none does) -- so that `with_jni_context` can mark it
        // escalated, and the drop may go idle incrementally. Never without
        // the switch: `quiet` is `false` then.
        if vm_only && quiet && !local_refs_may_be_held_raw() {
            let owns = JNI_NATIVE_CALL
                .try_with(|t| {
                    if t.leaf.get() != LEAF_NONE {
                        return false;
                    }
                    t.leaf.set(LEAF_VM_ONLY);
                    true
                })
                .unwrap_or(false);
            if owns {
                entry.foreign_quiet = true;
                entry.locals_mark = local_frames_mark();
            }
        }
        entry.foreign_body = shared.natives.jni_phase_census.start();
        entry.transition = Some(shared);
        entry
    }
}

/// How a JNIEnv function's entry puts its JNI native call back in native
/// (`Drop for ForeignJniEntry`).
pub(in crate::native::jni) enum Reenter {
    /// gcd d10/j: append what the function minted; no deposit
    /// ([`native_call_reenter_incremental`]), with the call's gch h4/bp band.
    Incremental(Option<(usize, usize)>),
    /// A full deposit ([`native_call_enter_native`]), with that band.
    Full(Option<(usize, usize)>),
    /// JIT round 21 wave 19 (lane jnilazy, FF3-3): the lazy state again over
    /// the call's lazy band ([`native_call_enter_lazy`]).
    Lazy((usize, usize)),
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
                            // gci i20/jt: by address (see `Self::native`).
                            slot.as_ref().map(|r| (Arc::as_ptr(&r.shared), r.thread))
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
            if raw_local_escapes() != 0 {
                count_attachment_known_raw_locals(Arc::clone(shared.mem.gc_barrier.pause_ledger()));
            }
            shared
                .natives
                .jni_phase_census
                .stop(JniPhase::ForeignBody, self.foreign_body.take());
            // gci i5/fj: give back this entry's `LEAF_VM_ONLY` state and learn
            // whether the function stayed off every path to Java; then go idle
            // incrementally when nothing else changed, else the full way.
            let quiet_clean = std::mem::take(&mut self.foreign_quiet)
                && JNI_NATIVE_CALL
                    .try_with(|t| {
                        let state = t.leaf.get();
                        if state == LEAF_VM_ONLY || state == LEAF_VM_ONLY_ESCALATED {
                            t.leaf.set(LEAF_NONE);
                        }
                        state == LEAF_VM_ONLY
                    })
                    .unwrap_or(false);
            if !(quiet_clean && foreign_enter_idle_incremental(&shared, self.locals_mark)) {
                foreign_enter_idle(&shared);
            }
            let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
        }
        if let Some((shared, thread)) = self.native.take() {
            // SAFETY: gci i20/jt -- the record that owns an `Arc` of this VM
            // outlives the entry (see `ForeignJniEntry::native`).
            let shared: &SharedVm = unsafe { &*shared };
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
                    // JIT round 21 wave 19 (lane jnilazy, FF3-3; JNILAZY21R-1):
                    // a lazy call goes back lazily over its own band even
                    // when it holds local references (`begin_native_region_lazy_with_locals`
                    // marks `locals_empty = false` so a claiming pause pins and
                    // reads `JNI_LOCAL_FRAMES`).
                    if let Some(lazy_band) = rec.lazy {
                        return Some(Reenter::Lazy(lazy_band));
                    }
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
                    // gch h4/bp: the call's band, for either way back in.
                    Some(if incremental {
                        Reenter::Incremental(rec.band)
                    } else {
                        Reenter::Full(rec.band)
                    })
                })
                .ok()
                .flatten();
            match reenter {
                Some(Reenter::Lazy(lazy_band)) => {
                    // SAFETY: as above.
                    if !native_call_enter_lazy(&shared, unsafe { &mut *thread }, lazy_band) {
                        // The context declined (the heap or the chain no
                        // longer allows a claim): the package's way, from a
                        // fresh `sync`, for the rest of the call.
                        let band = demote_lazy_record(thread);
                        // SAFETY: as above.
                        native_call_enter_native(&shared, unsafe { &mut *thread }, band);
                    }
                }
                Some(Reenter::Incremental(band)) => {
                    // gch h4/bp: the quiet leave kept the band with the
                    // origins; stored again so the flag the re-entry raises
                    // can never go up over another call's band.
                    // SAFETY: as above; atomics of the thread's block state.
                    store_in_native_band(&unsafe { &*thread }.gc_block_state, band);
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
                Some(Reenter::Full(band)) => {
                    // SAFETY: as above.
                    native_call_enter_native(&shared, unsafe { &mut *thread }, band);
                }
                None => {
                    if owned_vm_only {
                        // The call stays counted: the origins the quiet leave
                        // kept describe no blocked window any more.
                        // SAFETY: as above; the lock is the field's own.
                        unsafe { &*thread }
                            .gc_block_state
                            .slot_origins
                            .lock()
                            .clear();
                        // gch h4/bp: nor does the band.
                        // SAFETY: as above.
                        clear_in_native_band(&unsafe { &*thread }.gc_block_state);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A Java thread inside a JNI native method (`CRATONVM_JNI_NATIVE_TRANSITIONS`,
// default ON on every backend since the user flip of 2026-10-13, WARNING
// gci-user-flip-20261013; `=0` kills it -- gci i19/jt: this said "default OFF")
// ---------------------------------------------------------------------------
//
// gcd d3/k, `docs/feature-designs/real-jdk-mode.md`.
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
// use-after-move of `common-w2c-jni-local-refs-are-raw-addresses-FIXED-20261010.md`, widened
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
//   `docs/feature-designs/real-jdk-mode.md`.
// No bracket publishes the code-reclamation blocked-stack summary any more
// (`GcBarrier::mark_in_native_enter`): a thread in native holds retired
// compiled bodies as it does with the flag off.

/// `CRATONVM_JNI_NATIVE_TRANSITIONS`, latched like every flag. Default ON
/// since 2026-10-13 ([`native_transitions_default_from`]; gci i19/jt: this
/// said "Default OFF"); the value rule is [`indirect_locals_from`]'s, so `=0`
/// (or any off word) kills it. Inert unless locals are indirect ([`locals_are_indirect_here`]).
/// gcd d10/j: it is also the package switch of the other two JNI flags
/// ([`jni_switches`]).
pub(in crate::native::jni) fn native_transitions_active() -> bool {
    if let Some(on) = native_transitions_test_override() {
        return on;
    }
    jni_switches().native_transitions
}

/// JIT round 16 wave 12 (lane inpkg): are the in-native door fixes on
/// ([`in_native_doors_from`])? Never without the native transitions.
pub(in crate::native::jni) fn in_native_doors_active() -> bool {
    native_transitions_active() && jni_switches().in_native_doors
}

/// JIT round 16 wave 12 (lane inpkg): what [`enter_vm_for_embedding_call`]
/// holds; the call goes back into native when it drops.
#[must_use = "the call goes back into native when the entry drops"]
pub struct EmbeddingCallEntry {
    pub(in crate::native::jni) _entry: ForeignJniEntry,
}

/// JIT round 16 wave 12 (lane inpkg): the door of the embedding library's
/// flat `cratonvm_*` API (`libcratonvm`'s `with_vm`). C code inside a JNI
/// native in native (`CRATONVM_JNI_NATIVE_TRANSITIONS`) that calls one --
/// `cratonvm_new_string`, `cratonvm_get_field`, `cratonvm_invoke_*` -- used
/// to touch the heap, or run Java, while every pause excluded the thread.
/// This takes the call out of native first, as a JNIEnv function does
/// ([`ForeignJniEntry::enter`]'s in-native half; the full re-entry on drop,
/// since such a call may run Java). Inert everywhere else, and without
/// [`in_native_doors_active`]: an idle foreign attachment and a GC-safe FFM
/// downcall keep what the flat API did before.
pub fn enter_vm_for_embedding_call() -> EmbeddingCallEntry {
    let entry = if in_native_doors_active() {
        ForeignJniEntry::enter_from_native(false).unwrap_or_else(ForeignJniEntry::inert)
    } else {
        ForeignJniEntry::inert()
    };
    EmbeddingCallEntry { _entry: entry }
}

/// Is every local handed to native code on this thread an indirect handle?
/// The rule [`record_local_handle_indirect`] encodes by.
pub(in crate::native::jni) fn locals_are_indirect_here() -> bool {
    indirect_locals_active() && (!is_foreign_attached() || foreign_transitions_active())
}

/// gcj j2/jn2 (cut 1 of
/// `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts-20261019`):
/// `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`, default ON since 2026-10-26 (t1, `=0` kills),
/// read once per VM ([`JniSwitches::read_flags`]). With it, a JNI native
/// dispatch ([`dispatch_jni_native_census`]) resolves the VM's [`JniSwitches`]
/// once and passes them down -- the receiver's and the arguments' local
/// recording, the band check, the in-native entry ([`JniNativeCall`]) -- and
/// the entry and leave read the VM they hold ([`jni_switches_of`]) instead of
/// resolving them again through the thread's JNI context (a TLS `RefCell`
/// borrow and a struct copy each, about seven per call before). The values
/// are the same on every production path (the dispatch's context VM is the
/// VM the entry records); only the number of resolutions changes. Off, every
/// site resolves as before.
pub(in crate::native::jni) fn switches_once_per_call_enabled() -> bool {
    // WARNING t1-user-flip-20261026: default ON, `=0` kills.
    true
}

/// gcj j2/jn2 (cut 1): [`native_transitions_active`] over resolved switches
/// (the unit-test override still wins).
pub(in crate::native::jni) fn native_transitions_active_with(switches: &JniSwitches) -> bool {
    native_transitions_test_override().unwrap_or(switches.native_transitions)
}

/// gcj j2/jn2 (cut 1): [`indirect_locals_active`] over resolved switches.
pub(in crate::native::jni) fn indirect_locals_active_with(switches: &JniSwitches) -> bool {
    indirect_locals_test_override().unwrap_or(switches.indirect_locals)
}

/// gcj j2/jn2 (cut 1): [`locals_are_indirect_here`] over resolved switches.
pub(in crate::native::jni) fn locals_are_indirect_here_with(switches: &JniSwitches) -> bool {
    indirect_locals_active_with(switches)
        && (!is_foreign_attached()
            || foreign_transitions_test_override().unwrap_or(switches.foreign_transitions))
}

/// How many local refs this thread handed to native code RAW although
/// [`locals_are_indirect_here`] held (no open frame, or a slot past the
/// encodable range). Monotonic; compared, never reset. `pub(crate)` for a
/// caller that brackets a C call outside `dispatch_jni_native` in a
/// [`JniNativeCall`] (`docs/feature-designs/real-jdk-mode.md`).
pub(crate) fn raw_local_escapes() -> u64 {
    JNI_NATIVE_CALL
        .try_with(|t| t.raw_escapes.get())
        .unwrap_or(0)
}

/// Is this thread inside a JNI native call that is in native right now
/// ([`JniNativeCall`])? Never with `CRATONVM_JNI_NATIVE_TRANSITIONS` off.
pub(in crate::native::jni) fn in_native_jni_call() -> bool {
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
pub(in crate::native::jni) fn note_raw_local_escape() {
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
    // An initially indirect foreign attachment can hand out a raw fallback
    // after attach (for example an unencodable local slot). Count it before
    // the entry can go idle, so an old-space major cannot slide that handle.
    if FOREIGN_ATTACH_FRAME.try_with(|c| c.get().is_some()).unwrap_or(false) {
        let _ = with_shared_vm(|shared| {
            count_attachment_known_raw_locals(Arc::clone(shared.mem.gc_barrier.pause_ledger()));
        });
    }

}

/// The in-native record of the innermost JNI native call on this thread that
/// [`JniNativeCall`] took into native.
pub(in crate::native::jni) struct NativeCallRecord {
    /// The VM the call runs in (the dispatch's context; kept here because a
    /// `DetachCurrentThread` from the native clears the JNI context).
    pub(in crate::native::jni) shared: Arc<SharedVm>,
    /// The dispatching thread's `JvmThread` (the dispatch's `JNI_THREAD`).
    pub(in crate::native::jni) thread: *mut JvmThread,
    /// `true` while the thread is in native (GC-blocked); `false` while a
    /// JNIEnv function runs, or for good once a raw local escaped.
    pub(in crate::native::jni) in_native: bool,
    /// [`raw_local_escapes`] when the call went into native.
    pub(in crate::native::jni) raw_escapes: u64,
    /// gcd d5/f: the dispatching thread's `gc_block_state`
    /// (`Arc::as_ptr(&thread.gc_block_state)`), valid for the record's whole
    /// life: the `Arc` is owned by `*thread`, which outlives the record. A raw
    /// pointer so the leaf path reads its fold count without forming a
    /// reference to the `JvmThread` the suspended interpreter holds mutably.
    pub(in crate::native::jni) block_state: *const crate::threading::jvm_thread::GcBlockState,
    /// gcd d5/f: [`NativeSync`] read just before the call last went into
    /// native (its deposit). Equal to a fresh read = nothing this thread holds
    /// has moved since, and its deposited snapshot is still exact.
    pub(in crate::native::jni) sync: NativeSync,
    /// gcd d10/j: the thread's `redefinitions_seen` at the last FULL deposit.
    /// A quiet leave converts obsolete frames when a class was redefined
    /// (`convert_obsolete_frames_if_redefined` advances the field), after
    /// which that deposit no longer describes the frames.
    pub(in crate::native::jni) deposit_redefinitions: u64,
    /// gcd d10/j: snapshot entries [`native_call_reenter_incremental`] appended
    /// since the last full deposit; at [`INCREMENTAL_APPEND_LIMIT`] the next
    /// re-entry is a full deposit again, which drops what deleted locals kept.
    pub(in crate::native::jni) appended: u32,
    /// gch h4/bp: the compiled band `[lo, hi)` this call left to the pause
    /// ([`in_native_band_at_pause`]), decided once at the call's entry, where
    /// the dispatch captured its caller's callee-saved registers inside it.
    /// Every full re-entry of the call deposits with the same band: the
    /// frames above the native's C code are the same frames. `None`: every
    /// deposit of the call scans its compiled frames itself.
    pub(in crate::native::jni) band: Option<(usize, usize)>,
    /// JIT round 21 wave 19 (lane jnilazy, FF3-3): `Some([lo, hi))` while the
    /// call's in-native state is the LAZY one ([`native_call_enter_lazy`]: no
    /// deposit, `in_blocked_region` down, a pause claims the thread) over that
    /// stack band; `None` for the package's deposit-based state. Leaves and
    /// re-entries follow it ([`native_call_leave_lazy`]); set to `None` for
    /// the rest of the call once the call can no longer go back lazily (a
    /// local reference now exists, or the context declined).
    pub(in crate::native::jni) lazy: Option<(usize, usize)>,
}

/// gcd d10/j: how many snapshot entries incremental re-entries may append
/// before the next re-entry deposits in full. Each append keeps the referents
/// of every local of the call alive until that full deposit (over-retention
/// only, never a lost root); a `NewStringUTF` + `DeleteLocalRef` loop appends
/// one entry per round, so this is one full deposit per ~256 rounds.
pub(in crate::native::jni) const INCREMENTAL_APPEND_LIMIT: u32 = 256;

/// gcd d5/f: "has anything happened to this thread's roots since it went into
/// native?" as two counters: the VM's pause generation (any completed pause)
/// and the thread's fold count (any relocation folded into its blocked state,
/// `GcBlockState::blocked_folds`). Relocations run only inside pauses, and a
/// pause never completes without advancing the generation, so the fold count
/// is the belt to the generation's braces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::native::jni) struct NativeSync {
    pub(in crate::native::jni) generation: u64,
    pub(in crate::native::jni) folds: u64,
}

impl NativeSync {
    #[inline]
    pub(in crate::native::jni) fn read(shared: &SharedVm, block: &crate::threading::jvm_thread::GcBlockState) -> Self {
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
pub(in crate::native::jni) const LEAF_NONE: u8 = 0;
/// A leaf JNIEnv function runs in an open window.
pub(in crate::native::jni) const LEAF_OPEN: u8 = 1;
/// A leaf function escalated to the full transition
/// ([`escalate_leaf_window`]): the call is out of native until the
/// function's entry drops.
pub(in crate::native::jni) const LEAF_ESCALATED: u8 = 2;
/// gcd d10/j: a function entered through [`ForeignJniEntry::enter_vm_only`]
/// left native quietly and runs, counted, without having reached Java.
pub(in crate::native::jni) const LEAF_VM_ONLY: u8 = 3;
/// gcd d10/j: that function reached [`with_jni_context`] (it builds an
/// exception, or otherwise may run Java): its entry re-enters the full way.
pub(in crate::native::jni) const LEAF_VM_ONLY_ESCALATED: u8 = 4;

/// This thread's state for the in-native transitions: one thread-local (the
/// per-VM statics ratchet counts every declaration line).
pub(in crate::native::jni) struct NativeCallTls {
    /// See [`NativeCallRecord`]. `None` outside every in-native call.
    pub(in crate::native::jni) record: std::cell::RefCell<Option<NativeCallRecord>>,
    /// gcd d5/f: [`LEAF_NONE`] / [`LEAF_OPEN`] / [`LEAF_ESCALATED`]; see
    /// [`ForeignJniEntry::enter_leaf`]. gcd d10/j: also [`LEAF_VM_ONLY`] /
    /// [`LEAF_VM_ONLY_ESCALATED`] ([`ForeignJniEntry::enter_vm_only`]); at most
    /// one entry owns a non-`NONE` state at a time.
    pub(in crate::native::jni) leaf: Cell<u8>,
    /// gcd d5/f: this OS thread's leaf-window word, made at its first leaf
    /// window and registered with each barrier it opens one under. Marked
    /// dead when this thread-local is dropped (`Drop for NativeCallTls`).
    pub(in crate::native::jni) leaf_word: std::cell::OnceCell<Arc<crate::threading::gc_barrier::LeafWindowWord>>,
    /// See [`raw_local_escapes`].
    pub(in crate::native::jni) raw_escapes: Cell<u64>,
    /// gcd d4/k2: native dispatches on this thread in progress that were NOT
    /// counted as holding raw locals (their locals were indirect); see
    /// [`RawLocalsDispatch`].
    pub(in crate::native::jni) uncounted_dispatches: Cell<u32>,
    /// gcd d4/k2: this thread already opened its fail-closed raw-locals count
    /// for a raw escape inside an uncounted dispatch (see
    /// [`note_raw_local_escape`]).
    pub(in crate::native::jni) late_raw_opened: Cell<bool>,
    /// gcd d10/j (`gcd-d5f-jni-default-dispatch-costs-500ns`): the storage of
    /// the last local frame [`truncate_local_frames`] closed, emptied, kept
    /// for the next [`push_local_frame`] -- every JNI native dispatch opens and
    /// closes one implicit frame, which was a heap allocation and a free per
    /// call. Holds at most one `Vec` of at most [`SPARE_LOCAL_FRAME_MAX`]
    /// slots; never holds a handle (it is cleared before it is kept).
    pub(in crate::native::jni) spare_local_frame: Cell<Vec<JObject>>,
    /// gci i16/dp (i15/jk R1): the argument buffer of the last
    /// [`va_list_to_jvalues`] that [`free_jvalues`] handed back, kept for the
    /// next one -- every `Call<Type>MethodV` / variadic call with arguments
    /// built a `Vec<JValue>` and freed it. One buffer of
    /// [`VA_SPARE_SLOTS`]; a nested call while it is out allocates its own.
    pub(in crate::native::jni) va_spare: Cell<Option<Box<[JValue; VA_SPARE_SLOTS]>>>,
    /// gce e1/j: the last [`find_jni_native`] hit ([`JniFnMemo`]).
    pub(in crate::native::jni) fn_memo: std::cell::RefCell<Option<JniFnMemo>>,
    /// gcf f2/j: what the last FULL in-native deposit on this OS thread
    /// published for its interpreter frames, under the key it was valid for
    /// ([`InNativeRepeat`], `CRATONVM_JNI_IN_NATIVE_REPEAT`).
    pub(in crate::native::jni) repeat: std::cell::RefCell<Option<RepeatSaved>>,
    /// gcf f2/j: this OS thread called `AttachCurrentThread` with the
    /// foreign-attach gate off (the env-only attach, which registers nothing)
    /// and has not detached since: `GetEnv` answers `JNI_OK` for it, as
    /// HotSpot does for an attached thread ([`current_os_thread_is_vm_thread`]).
    pub(in crate::native::jni) env_only_attached: Cell<bool>,
    /// gcf f4/j: a `Weak` of the last VM this OS thread installed as its JNI
    /// context ([`shared_vm_arc`]).
    pub(in crate::native::jni) vm_weak: std::cell::RefCell<Option<Weak<SharedVm>>>,
    /// gcf f4/j: a root deposit (`deposit_root_snapshot_inner`) is running on
    /// this OS thread ([`RootDepositScope`]).
    pub(in crate::native::jni) root_deposit: Cell<bool>,
    /// gcj j1pn ledger item 4: a safepoint root publish
    /// (`update_root_snapshot`) is running on this OS thread
    /// ([`SafepointPublishScope`]).
    pub(in crate::native::jni) safepoint_publish: Cell<bool>,
    /// gci i5/fj: see [`ForeignIdleRecord`]. `None` with the switch off.
    pub(in crate::native::jni) foreign_idle: Cell<Option<ForeignIdleRecord>>,
    /// gci i9/jw: this OS thread's id, read once
    /// ([`publish_local_frames_owner`]); 0 until then.
    pub(in crate::native::jni) own_os_tid: Cell<u32>,
    /// gci i27/kd: [`jni_switches`]'s answer on this OS thread when it acts
    /// for no recorded VM (no JNI context, no foreign attachment), read from
    /// the flag snapshot at the first such call: a blocking native's wake
    /// refresh asks once per compiled frame.
    pub(in crate::native::jni) switches_memo: Cell<Option<JniSwitches>>,
    /// gci i5/fj: unit tests' per-thread value of [`foreign_quiet_idle_active`].
    #[cfg(test)]
    pub(in crate::native::jni) quiet_idle_override: Cell<Option<bool>>,
    /// Unit tests' per-thread switch of [`native_transitions_active`].
    #[cfg(test)]
    pub(in crate::native::jni) test_override: Cell<Option<bool>>,
    /// Unit tests' per-thread switch of [`raw_locals_counted`].
    #[cfg(test)]
    pub(in crate::native::jni) raw_locals_override: Cell<Option<bool>>,
    /// gcf f2/j: unit tests' per-thread answer of
    /// [`current_os_thread_is_vm_thread`] (a test thread is no VM thread).
    #[cfg(test)]
    pub(in crate::native::jni) vm_thread_override: Cell<Option<bool>>,
    /// gcf f2/j: unit tests' per-thread answer of [`repeat_process_terms`].
    #[cfg(test)]
    pub(in crate::native::jni) repeat_terms_override: Cell<Option<(Option<(u64, u32)>, bool)>>,
    /// gch h1/jw: unit tests' per-thread value of the
    /// `CRATONVM_JNI_FOREIGN_TID_PUBLISH` half of [`foreign_tid_publish_active`].
    #[cfg(test)]
    pub(in crate::native::jni) tid_publish_override: Cell<Option<bool>>,
    /// JIT round 21 wave 20 (lane jniharness, JNI6-1): unit tests' per-thread
    /// answer of [`jni_switches`] (the JNI conformance harness's arms).
    #[cfg(test)]
    pub(in crate::native::jni) switches_override: Cell<Option<JniSwitches>>,
}

impl NativeCallTls {
    pub(in crate::native::jni) const fn new() -> Self {
        NativeCallTls {
            record: std::cell::RefCell::new(None),
            leaf: Cell::new(LEAF_NONE),
            leaf_word: std::cell::OnceCell::new(),
            raw_escapes: Cell::new(0),
            uncounted_dispatches: Cell::new(0),
            late_raw_opened: Cell::new(false),
            spare_local_frame: Cell::new(Vec::new()),
            va_spare: Cell::new(None),
            fn_memo: std::cell::RefCell::new(None),
            repeat: std::cell::RefCell::new(None),
            env_only_attached: Cell::new(false),
            vm_weak: std::cell::RefCell::new(None),
            root_deposit: Cell::new(false),
            safepoint_publish: Cell::new(false),
            foreign_idle: Cell::new(None),
            own_os_tid: Cell::new(0),
            switches_memo: Cell::new(None),
            #[cfg(test)]
            quiet_idle_override: Cell::new(None),
            #[cfg(test)]
            test_override: Cell::new(None),
            #[cfg(test)]
            raw_locals_override: Cell::new(None),
            #[cfg(test)]
            vm_thread_override: Cell::new(None),
            #[cfg(test)]
            repeat_terms_override: Cell::new(None),
            #[cfg(test)]
            tid_publish_override: Cell::new(None),
            #[cfg(test)]
            switches_override: Cell::new(None),
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
pub(in crate::native::jni) fn escalate_leaf_window() {
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
pub(in crate::native::jni) fn escalate_leaf_window_slow() {
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
                    // gci i20/jt: by address (see `ForeignJniEntry::native`).
                    Some((Arc::as_ptr(&rec.shared), rec.thread, rec.sync))
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
        // SAFETY: the record, which owns an `Arc` of this VM, is still
        // installed (the leaf function runs inside its call).
        let shared: &SharedVm = unsafe { &*shared };
        // SAFETY: as in `ForeignJniEntry::enter_from_native`: the record's
        // `JvmThread` outlives it, and the leaf function holds no `&mut` to it.
        let _ = native_call_leave_native(shared, unsafe { &mut *thread }, sync, false);
    }
}

