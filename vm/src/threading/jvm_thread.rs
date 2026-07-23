// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread JVM execution state.
//!
//! Each Java thread has its own `JvmThread` containing:
//! - Call stack (for stack traces)
//! - Test output buffer (`printed`)
//! - Thread identity and flags
//
// T1.8.2 — production-code panic gate. Per-thread state is on the
// hottest paths in the VM; an `.unwrap()` here would panic the entire
// runtime. Test code is allowed unwraps so the gate is opt-out under
// `#[cfg(test)]`.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,)
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar as PLCondvar, Mutex as PLMutex};

use crate::classloading::resolution::InvokeCache;
use crate::runtime::frame::Frame;
use crate::runtime::fx_collections::FxHashMap;
use crate::types::{ObjectRef, Value};
use cratonvm_types::ClassId;

/// Pool type for SoA locals and stack vecs: (values, tags).
pub type SoaPool = Vec<(Vec<u64>, Vec<u8>)>;

/// A two-entry per-thread cache for repeated ASCII case conversion.  Returning
/// alternating immutable results avoids the observable same-object shortcut
/// while eliminating allocation/collection in hot use-and-discard patterns.
#[derive(Clone)]
pub struct StringCaseCacheEntry {
    pub source: ObjectRef,
    pub upper: bool,
    pub first: ObjectRef,
    pub second: ObjectRef,
    pub next: bool,
}

#[derive(Clone)]
pub struct JitHashMapStringNodeCacheEntry {
    pub map: ObjectRef,
    pub node: ObjectRef,
    pub key: String,
    pub mod_count_slot: usize,
    pub mod_count: i32,
}

// Pool size limits — prevent unbounded growth
const MAX_POOL_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// GcBlockState — blocked-region GC maintenance state (shared with registry)
// ---------------------------------------------------------------------------

/// Blocked-region GC state for one thread, shared `JvmThread` ↔ `ThreadRegistry`
/// (same pattern as `root_snapshot`).
///
/// A thread parked in a blocking native (`Object.wait`, `Thread.join`,
/// `LockSupport.park`, `ReferenceQueue.remove`, …) is excluded from the
/// stop-the-world barrier (`GcBarrier::threads_blocked`), so any number of
/// GCs can complete while it sleeps. Two things would otherwise go stale:
///
/// 1. its deposited `root_snapshot` — scanned as roots by every GC; after the
///    first missed *moving* collection the snapshot addresses point into a
///    vacated semispace, and once that space cycles back around and is
///    collected again the collector evacuates garbage "objects" through
///    them (writing forwarding state into the interior of innocent live
///    objects — the H2 TestScript stale-receiver SEGV);
/// 2. its frames — `check_post_block_gc` only applied the pointer map when a
///    STW was active at the exact wake instant; a GC that completed mid-block
///    left every local/operand-stack ref pointing at recycled from-space.
///
/// The GC initiator therefore calls
/// `ThreadRegistry::fold_pointer_map_into_blocked` (`update_all_roots`
/// step 20, under STW) for every thread whose `in_blocked_region` flag is
/// set: it remaps the thread's `root_snapshot` in place and composes the
/// GC's pointer map into `fixup` (chaining `orig → cur → new` across
/// multiple missed GCs, keyed by the address the frames still hold). On
/// wake the thread applies and clears `fixup` in `check_post_block_gc`.
/// cceres3: one precisely-tracked frame slot for the blocked window — see
/// `GcBlockState::slot_origins`.
#[derive(Clone, Copy)]
pub struct SlotOrigin {
    pub frame: u32,
    pub idx: u32,
    pub is_stack: bool,
    /// Address the slot held at the blocking deposit.
    pub orig: usize,
    /// The object's current address, advanced by every GC initiator's fold
    /// through that collection's pointer map (exact per-map lookup).
    pub cur: usize,
}

pub struct GcBlockState {
    /// True from `deposit_root_snapshot` (just before the thread blocks)
    /// until the end of `check_post_block_gc` (after the fixup is applied).
    pub in_blocked_region: AtomicBool,
    /// Java-visible blocking kind while `in_blocked_region` is true:
    /// 1 = WAITING (wait/park/join), 2 = BLOCKED (monitor acquisition).
    /// The GC protocol only needs the boolean above; preserving this small
    /// distinction lets `Thread.getState()` report the JDK state correctly.
    pub java_state: AtomicU8,
    /// Composed `frame-held address → current address` map accumulated by GC
    /// initiators for every collection that completed while the thread was
    /// in a blocked region. Applied + cleared on wake.
    pub fixup: PLMutex<std::collections::HashMap<usize, usize>>,
    /// cceres3 (WildFly boot stale-frame family): exact per-slot tracking for
    /// the blocked window. `fixup` above is keyed by the address the frames
    /// held when each object FIRST moved — a chain that breaks if any link's
    /// seed was missed (filtered snapshot, multi-block chains, recycled-address
    /// ABA), permanently stranding the slot. Each entry here instead pins down
    /// one (frame, slot) with the address it held at the blocking deposit;
    /// folds advance `cur` with an exact per-collection lookup and the wake
    /// write-back stores `cur` straight into the slot. Filled only by the
    /// flag-raising deposit; taken (and cleared) on wake.
    pub slot_origins: PLMutex<Vec<SlotOrigin>>,
}

impl GcBlockState {
    pub fn new() -> Self {
        Self {
            in_blocked_region: AtomicBool::new(false),
            java_state: AtomicU8::new(0),
            fixup: PLMutex::new(std::collections::HashMap::new()),
            slot_origins: PLMutex::new(Vec::new()),
        }
    }
}

impl Default for GcBlockState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ParkState — binary semaphore for LockSupport.park() / unpark()
// ---------------------------------------------------------------------------

/// Per-thread parking permit for `LockSupport.park()` / `unpark()`.
///
/// Implements a binary semaphore: `unpark()` sets the permit, `park()` consumes
/// it (or blocks until one is available). If `unpark()` is called before
/// `park()`, the next `park()` returns immediately.
pub struct ParkState {
    mutex: PLMutex<bool>,
    condvar: PLCondvar,
    /// DBG (`CRATONVM_DBG_PARKLAT`): monotonic nanos of the most recent
    /// `unpark()` that SET the permit (0 = none). `park_interruptible`
    /// reads it on wake-with-permit and reports the unpark→wake latency
    /// when it exceeds a threshold, separating "the signal was generated
    /// late" (Java-side / protocol) from "the signal was delivered late"
    /// (VM park machinery) in the RRWL crawl/join-stall investigation.
    last_unpark_nanos: std::sync::atomic::AtomicU64,
}

/// Cached `CRATONVM_DBG_PARKLAT` gate.
#[inline]
fn parklat_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_PARKLAT").is_some())
}

/// Monotonic nanos for the PARKLAT diagnostic (process-relative).
#[inline]
fn parklat_now_nanos() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}

impl ParkState {
    /// Create a new ParkState with no permit available.
    pub fn new() -> Self {
        Self {
            mutex: PLMutex::new(false),
            condvar: PLCondvar::new(),
            last_unpark_nanos: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Consume the permit or block until one is available.
    ///
    /// If a permit is available (from a prior `unpark()`), consumes it and
    /// returns immediately. Otherwise, blocks until `unpark()` is called or
    /// the timeout expires.
    pub fn park(&self, timeout: Option<std::time::Duration>) {
        let mut permit = self.mutex.lock();
        if *permit {
            *permit = false;
            return;
        }
        match timeout {
            Some(dur) if !dur.is_zero() => {
                self.condvar.wait_for(&mut permit, dur);
            }
            None => {
                self.condvar.wait(&mut permit);
            }
            _ => {} // zero timeout = no-op
        }
        *permit = false; // consume permit even on timeout/spurious wakeup
    }

    /// Consume the permit or block until one is available, with interrupt awareness.
    ///
    /// Like `park()`, but periodically checks the interrupt flag so that
    /// `Thread.interrupt()` can break a parked thread out of the wait.
    pub fn park_interruptible(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        let mut permit = self.mutex.lock();
        if *permit {
            *permit = false;
            return;
        }
        // Check interrupt before blocking
        if interrupted.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let poll = std::time::Duration::from_millis(5);
        match timeout {
            Some(dur) if !dur.is_zero() => {
                let deadline = std::time::Instant::now() + dur;
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() || *permit {
                        break;
                    }
                    let wait_time = remaining.min(poll);
                    self.condvar.wait_for(&mut permit, wait_time);
                    if *permit || interrupted.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
            }
            None => {
                // Untimed park — poll for interrupt or unpark
                loop {
                    self.condvar.wait_for(&mut permit, poll);
                    if *permit || interrupted.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
            }
            _ => {} // zero timeout = no-op
        }
        // DBG (CRATONVM_DBG_PARKLAT): report a tardy delivery — the wake
        // observed the permit long after the unpark that set it.
        if *permit && parklat_enabled() {
            let set_at = self
                .last_unpark_nanos
                .load(std::sync::atomic::Ordering::Acquire);
            if set_at != 0 {
                let lat_ms = parklat_now_nanos().saturating_sub(set_at) / 1_000_000;
                if lat_ms >= 50 {
                    eprintln!(
                        "[parklat] unpark->wake {lat_ms}ms (thread {:?})",
                        std::thread::current().id(),
                    );
                }
            }
        }
        *permit = false;
    }

    /// Make a permit available; unblock a parked thread.
    ///
    /// If the thread is currently parked, it will be unblocked. If not,
    /// the next call to `park()` will return immediately.
    pub fn unpark(&self) {
        let mut permit = self.mutex.lock();
        *permit = true;
        if parklat_enabled() {
            self.last_unpark_nanos
                .store(parklat_now_nanos(), std::sync::atomic::Ordering::Release);
        }
        self.condvar.notify_one();
    }
}

impl Default for ParkState {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a thread is a platform (OS) thread or a virtual thread (JEP 444).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    /// Traditional platform thread backed by an OS thread.
    Platform,
    /// Virtual thread (Java 21+) — lightweight, scheduled onto carrier threads.
    Virtual,
}

/// Unique identifier for a JVM thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thread-{}", self.0)
    }
}

/// Per-thread execution state.
///
/// Each Java thread (including the main thread) has its own `JvmThread`.
/// This struct holds all state that is local to a single thread of execution.
pub struct JvmThread {
    /// Unique thread identifier.
    pub thread_id: ThreadId,

    /// Human-readable thread name.
    pub name: String,

    /// Live execution frames. The last element is the currently executing frame.
    /// Frames are pushed on method entry and popped on method return.
    /// Stack traces are derived from frames on demand (in capture_stack_trace).
    pub frames: Vec<Frame>,

    /// Pool of reusable locals (values, tags) pairs (avoids allocation on recursive calls).
    pub locals_pool: SoaPool,

    /// Pool of reusable stack (values, tags) pairs (avoids allocation on recursive calls).
    pub stacks_pool: SoaPool,

    /// Values printed by `tempPrint` — used by integration tests.
    pub printed: Vec<Value>,

    /// Lines printed via System.out.println — captured as plain Rust strings.
    pub printed_lines: Vec<String>,

    /// Whether this is a daemon thread.
    pub daemon: bool,

    /// Thread interrupt flag (shared with ThreadRegistry for cross-thread access).
    pub interrupted: Arc<AtomicBool>,

    /// Cached Java Thread object on the heap for this thread.
    pub java_thread_obj: Option<ObjectRef>,

    /// Parking permit for LockSupport.park()/unpark().
    pub park_state: Arc<ParkState>,

    /// Root snapshot: shared with ThreadRegistry for GC root scanning across threads.
    /// Updated at safepoints and before blocking operations.
    pub root_snapshot: Arc<parking_lot::Mutex<Vec<ObjectRef>>>,

    /// Frame trace snapshot: shared with ThreadRegistry so another thread can
    /// read this thread's Java call stack (cross-thread `Thread.getStackTrace()`
    /// / `dumpThreads()`). Published at the same blocking deposit points as
    /// `root_snapshot`, so for a parked thread it reflects where it is stuck.
    /// Line-less (see `stackwalker::capture_frames_no_lines`) to stay lock-free.
    pub frame_trace: Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,

    /// Optional diagnostic breadcrumb shared with the thread registry. When
    /// `CRATONVM_DBG_VM_STATE=1` is set, long-running VM helper paths publish a
    /// compact state here so STW census can identify non-bytecode stalls.
    pub vm_state: Arc<parking_lot::Mutex<String>>,

    /// Opt-in root-snapshot cache (`CRATONVM_ROOTSNAP_CACHE`): per *frozen*
    /// frame, `((frame.seq, frame.exec_epoch), that frame's scanned GC roots)`,
    /// indexed parallel to `frames[0..rs_cache.len()]`. Lets
    /// `update_root_snapshot` reuse the deep, continuously-frozen frames and
    /// re-scan only the churning top. Valid only while `rs_cache_gen ==
    /// heap.collection_count()` (a GC may have moved/promoted objects,
    /// invalidating the cached addresses). The key is `(seq, exec_epoch)` — NOT
    /// `seq` alone: `seq` proves the frame was never popped, but a still-present
    /// frame can RE-EXECUTE and reassign its locals; `exec_epoch` (bumped on
    /// callee-return and local-slot writes) detects that so stale roots are never
    /// reused (see `Frame::seq` / `Frame::exec_epoch`). Empty/unused when the
    /// gate is off.
    pub rs_cache: Vec<((u64, u64), Vec<ObjectRef>)>,
    /// GC collection count at which `rs_cache` was built (move/promote generation).
    pub rs_cache_gen: u64,

    /// Blocked-region GC state: shared with ThreadRegistry so a GC initiator
    /// can maintain this thread's roots while it is parked in a blocking
    /// native (`Object.wait` / `Thread.join` / `LockSupport.park` /
    /// `ReferenceQueue.remove`). See [`GcBlockState`].
    pub gc_block_state: Arc<GcBlockState>,

    /// GC roots for `Value::Object` arguments popped from the operand stack into a
    /// Rust `Vec` while a registered native runs (`safe_native_call`). Those refs
    /// are no longer on the Java stack until the callee returns, so without this
    /// list a safepoint GC can collect them mid-native (Letsgo AV after CCE
    /// `enhance` returns the original `Class`).
    pub native_pin_roots: Vec<ObjectRef>,

    /// Unused objects reserved in one old-generation allocation batch for
    /// small native allocations. Keeping the pool on the owning thread makes
    /// it a normal GC root set; every GC snapshot/remap path treats these
    /// entries exactly like `native_pin_roots` until `alloc_object` hands one
    /// to a native callback.
    pub native_alloc_pool: Vec<ObjectRef>,
    /// `(class_id, slot_count)` shared by every entry in
    /// `native_alloc_pool`. `None` when the pool is empty.
    pub native_alloc_pool_layout: Option<(ClassId, usize)>,

    /// Object result from the last `safe_native_call`: either an object return
    /// value before the interpreter pushes it onto the operand stack, or a
    /// native-thrown Java exception before the interpreter routes it through a
    /// catch handler. Covers the cross-thread GC window after the call returns.
    pub native_pending_return: Option<ObjectRef>,

    /// Bounded JIT cache for immutable String keys in exact HashMaps. Entries
    /// are normal thread roots and are cleared lazily on a structural change.
    pub jit_hashmap_string_node_cache: Vec<JitHashMapStringNodeCacheEntry>,
    pub string_case_cache: Vec<StringCaseCacheEntry>,

    /// Thread-local invoke cache — maps (caller_class, cp_index) to resolved targets.
    /// No locking needed since each thread owns its cache.
    pub invoke_cache: InvokeCache,

    /// Thread-local cache for the vtable-fast native-shadow guard.
    ///
    /// On invoke-cache misses, `execute_invokevirtual_vtable_fast` checks whether
    /// the receiver class or one of its native-shadowed ancestors should cede
    /// dispatch to the native/intrinsic path. HQL/lambda-heavy workloads can miss
    /// the invoke cache repeatedly, and the old guard paid a native-registry hash
    /// lookup for every ancestor on every miss.
    ///
    /// Keyed by `(receiver_class_id, method_name_hash, descriptor_hash)` and
    /// bypassed whenever class redefinition is active, because redefine can
    /// suppress native shadows for woven bytecode.
    pub native_shadow_cache: FxHashMap<(u32, u64, u64), bool>,

    /// Whether this is a platform or virtual thread (JEP 444, Java 21).
    pub kind: ThreadKind,

    /// Pin depth for virtual threads (JEP 491).
    ///
    /// Each monitor enter increments this; each monitor exit decrements.
    /// When non-zero on a virtual thread, the thread is "pinned" to its
    /// carrier — attempting to park/sleep will keep the carrier blocked
    /// and emit a `jdk.VirtualThreadPinned` JFR event.
    pub pin_count: u32,

    /// Last pin reason (for JFR event classification).
    /// "Monitor", "NativeMethod", "ClassInit", or empty when not pinned.
    pub pin_reason: &'static str,

    /// Scoped value binding stack (JEP 446, Java 25).
    /// Each entry is (key_id, key_ref, bound_value). Searched top-to-bottom.
    ///
    /// Round-9 GC fix: `key_ref` is the `ScopedValue` ObjectRef that owns
    /// `key_id`. When set, the GC root scanner reports it so the key
    /// object cannot be collected while a binding for it is live (only
    /// the value used to be retained; the key itself could be reclaimed
    /// while user code still observed the binding via
    /// `ScopedValue.isBound()` / `Carrier.get`).  `None` for legacy
    /// callers that pre-date the with-key API — those paths fall back to
    /// the value-only behaviour and are no worse than before.
    pub scoped_values: Vec<(u64, Option<ObjectRef>, Value)>,

    /// Thread-local allocation buffer for lock-free young-gen allocation.
    pub tlab: cratonvm_gc::Tlab,

    /// Per-thread **shadow stack** of live object references for precise,
    /// rewritable GC roots inside JIT-compiled code (see
    /// `cratonvm_gc::shadow_stack`). JIT code pushes live oops onto it before
    /// a GC-capable call and reloads them after; the collector marks and
    /// rewrites the pushed slots precisely, which lets a *moving* young-gen
    /// collection run safely while JIT frames are live. Unallocated (empty)
    /// until the thread first enters JIT code with the mechanism enabled.
    pub shadow_stack: cratonvm_gc::shadow_stack::ShadowStack,

    /// T1.5.1 — pending asynchronous exception to deliver at the next
    /// safepoint.
    ///
    /// When `Some(throwable)`, the next call to `safepoint_check`
    /// clears this field and raises the throwable so it propagates
    /// through the interpreter's normal exception table walk.
    /// Used by `Thread.stop0` and by any future API that needs to
    /// deliver a Throwable to a running thread without corrupting
    /// stack state.
    ///
    /// The field is owned by the target thread; cross-thread setters
    /// (the `Thread.stop0` native) go through the thread registry
    /// which takes a brief lock and then sets this via an
    /// `AsyncExceptionSlot` (see `thread_registry.rs`).
    ///
    /// The stored `ObjectRef` must point to a live `Throwable`
    /// subclass. The field is cleared by the safepoint before raise,
    /// so a single `stop()` call delivers exactly once.
    pub pending_async_exception: Option<ObjectRef>,

    /// T17.Δ.3 — JVMTI single-step enable for this thread.
    ///
    /// When set, the interpreter's per-instruction dispatch fires
    /// `JvmtiEventKind::SingleStep` once for each bytecode.  Set by
    /// `JvmtiEnv::set_event_notification_mode(Enable, SingleStep, Some(tid))`
    /// and cleared by the corresponding `Disable` call.
    ///
    /// Stored as `AtomicBool` because the flag may be flipped from a
    /// debugger thread while the owning thread is executing bytecode; the
    /// owning thread reads it with `Ordering::Relaxed` on the dispatch-loop
    /// hot path (single-thread view of its own flag is always consistent).
    pub single_step_enabled: AtomicBool,

    /// T17.Δ.5 — per-thread JVMTI frame-pop requests.
    ///
    /// Each entry is a frame depth (index into `frames`) for which
    /// `NotifyFramePop` was called.  When the interpreter is about to drop
    /// a frame it consults this list; if the frame's depth matches any
    /// registered request, `FramePop` fires and the entry is removed.
    ///
    /// Implemented as a small `Vec<u32>` because in practice only a handful
    /// of frames are marked at any given time (debugger "step out" uses
    /// one; profilers typically use zero).  Linear scan cost is negligible
    /// next to the frame-pop work.
    pub frame_pop_requests: Vec<u32>,
}

impl JvmThread {
    /// Byte offset of the `tlab` field from the start of `JvmThread`.
    ///
    /// Used by the JIT (`jit/src/x64.rs` `new` opcode) to emit an inline
    /// TLAB bump-pointer fast path. The JIT loads the per-thread TLAB
    /// cursor/end at `JvmThread base + TLAB_OFFSET + Tlab::CURSOR/END_OFFSET`.
    ///
    /// **MSRV note**: this crate targets Rust 1.75. `core::mem::offset_of!`
    /// is `const` only on 1.77+, so we compute the offset lazily on first
    /// access via a sentinel `JvmThread::default()` and cache it in a
    /// [`OnceLock`]. The computation is a single subtraction of two
    /// `&raw const` pointers — no allocation beyond the one-time default
    /// thread.
    ///
    /// Callers should treat the returned value as effectively `const` and
    /// cache it themselves (e.g. into a `JitRuntimeHelpers` field at
    /// helper-table init).
    pub fn tlab_offset() -> usize {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<usize> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let t = JvmThread::default();
            let base = &t as *const JvmThread as usize;
            let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
            tlab_addr - base
        })
    }

    /// Byte offset of the `shadow_stack` field from the start of `JvmThread`.
    ///
    /// Used by the JIT (`jit/src/x64.rs`) to emit the inline shadow-stack push
    /// at GC-capable safepoints: the codegen loads/stores the shadow `top` at
    /// `JvmThread base + shadow_stack_offset() + ShadowStack::TOP_OFFSET`.
    /// Computed lazily like [`Self::tlab_offset`] (MSRV: no const `offset_of!`).
    pub fn shadow_stack_offset() -> usize {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<usize> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let t = JvmThread::default();
            let base = &t as *const JvmThread as usize;
            let ss_addr = &t.shadow_stack as *const cratonvm_gc::shadow_stack::ShadowStack as usize;
            ss_addr - base
        })
    }

    /// Whether VM-state breadcrumbs should be published.
    #[inline]
    pub fn vm_state_diagnostics_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var_os("CRATONVM_DBG_VM_STATE").is_some())
    }

    /// Publish a short diagnostic state for STW census. No-op unless enabled.
    #[inline]
    pub fn set_vm_state<S: Into<String>>(&self, state: S) {
        if Self::vm_state_diagnostics_enabled() {
            *self.vm_state.lock() = state.into();
        }
    }

    /// Create a new thread with the given id and name.
    pub fn new(thread_id: ThreadId, name: &str) -> Self {
        Self {
            thread_id,
            name: name.to_string(),
            frames: Vec::new(),
            locals_pool: Vec::new(),
            stacks_pool: Vec::new(),
            printed: Vec::new(),
            printed_lines: Vec::new(),
            daemon: false,
            interrupted: Arc::new(AtomicBool::new(false)),
            java_thread_obj: None,
            park_state: Arc::new(ParkState::new()),
            root_snapshot: Arc::new(parking_lot::Mutex::new(Vec::new())),
            frame_trace: Arc::new(parking_lot::Mutex::new(Vec::new())),
            vm_state: Arc::new(parking_lot::Mutex::new(String::new())),
            rs_cache: Vec::new(),
            rs_cache_gen: 0,
            gc_block_state: Arc::new(GcBlockState::new()),
            native_pin_roots: Vec::new(),
            native_alloc_pool: Vec::new(),
            native_alloc_pool_layout: None,
            native_pending_return: None,
            jit_hashmap_string_node_cache: Vec::new(),
            string_case_cache: Vec::new(),
            invoke_cache: InvokeCache::new(),
            native_shadow_cache: FxHashMap::default(),
            kind: ThreadKind::Platform,
            pin_count: 0,
            pin_reason: "",
            scoped_values: Vec::new(),
            tlab: cratonvm_gc::Tlab::empty(),
            shadow_stack: cratonvm_gc::shadow_stack::ShadowStack::empty(),
            pending_async_exception: None,
            single_step_enabled: AtomicBool::new(false),
            frame_pop_requests: Vec::new(),
        }
    }

    /// Return a popped frame's Vec allocations to the pool for reuse.
    pub fn recycle_frame(&mut self, frame: Frame) {
        if self.locals_pool.len() < MAX_POOL_SIZE {
            frame.recycle(&mut self.locals_pool, &mut self.stacks_pool);
        }
        // If pool is full, frame's Vecs are simply dropped
    }

    /// T10.7 — recycle a frame, spilling any overflow into the VM-wide
    /// `VecPool`s on `SharedVm`.
    ///
    /// Semantics:
    ///   * When the thread-local `locals_pool` / `stacks_pool` has room, the
    ///     frame's Vecs are retained there exactly as before (zero locking).
    ///   * Once the thread-local pool is full (`MAX_POOL_SIZE`), we keep the
    ///     frame's Vecs alive by handing them to the VM-wide shared pools
    ///     (`operand_stack_pool` for the `Vec<u64>` halves and `tag_pool` for
    ///     the `Vec<u8>` halves).  Sibling threads later pull from the shared
    ///     pool when their own thread-local pool is empty (see
    ///     `refill_pools_from_shared`).
    ///
    /// This preserves allocations across thread boundaries without impacting
    /// the hot path — the shared mutex is only touched on the cold
    /// "thread-local pool full" edge.
    pub fn recycle_frame_with_shared(
        &mut self,
        frame: Frame,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) {
        // Split the frame's inner Vecs out of it exactly once.
        let (local_vals, local_tags, stack_vals, stack_tags) = frame.take_pool_parts();
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
            return;
        }
        // Thread-local pool is full — spill into the shared pools.
        operand_stack_pool.release(local_vals);
        tag_pool.release(local_tags);
        operand_stack_pool.release(stack_vals);
        tag_pool.release(stack_tags);
    }

    /// T10.7 — replenish the thread-local `locals_pool` / `stacks_pool` from
    /// the VM-wide shared pools when empty.
    ///
    /// Invoked just before a frame-push that will consume pooled Vecs so the
    /// shared-pool mutex is touched only on the cold refill path, not per call.
    pub fn refill_pools_from_shared(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
        max_locals: usize,
        max_stack: usize,
    ) {
        if self.locals_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_locals);
            let tags = tag_pool.acquire(max_locals);
            self.locals_pool.push((vals, tags));
        }
        if self.stacks_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_stack);
            let tags = tag_pool.acquire(max_stack);
            self.stacks_pool.push((vals, tags));
        }
    }
}

impl Default for JvmThread {
    /// Creates a default "main" thread with id 0.
    /// Used with `std::mem::take` for borrow-splitting.
    fn default() -> Self {
        Self::new(ThreadId(0), "main")
    }
}

impl std::fmt::Debug for JvmThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JvmThread")
            .field("thread_id", &self.thread_id)
            .field("name", &self.name)
            .field("frames_depth", &self.frames.len())
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JIT contract — `JvmThread::tlab_offset()` must report the actual
    /// byte offset of the `tlab` field. The JIT-emitted inline bump in
    /// `jit/src/x64.rs` reads `[thread + tlab_offset + CURSOR_OFFSET]`,
    /// so a wrong answer here corrupts the heap.
    #[test]
    fn tlab_offset_matches_field_address() {
        let t = JvmThread::default();
        let base = &t as *const JvmThread as usize;
        let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
        let expected = tlab_addr - base;
        assert_eq!(
            JvmThread::tlab_offset(),
            expected,
            "JvmThread::tlab_offset() out of sync with the actual field offset"
        );
        // Caching: a second call must return the same value (OnceLock).
        assert_eq!(JvmThread::tlab_offset(), expected);
    }

    #[test]
    fn thread_creation() {
        let thread = JvmThread::new(ThreadId(1), "worker-1");
        assert_eq!(thread.thread_id, ThreadId(1));
        assert_eq!(thread.name, "worker-1");
        assert!(thread.frames.is_empty());
        assert!(thread.printed.is_empty());
        assert!(!thread.daemon);
    }

    #[test]
    fn thread_default_is_main() {
        let thread = JvmThread::default();
        assert_eq!(thread.thread_id, ThreadId(0));
        assert_eq!(thread.name, "main");
    }

    #[test]
    fn thread_id_display() {
        assert_eq!(format!("{}", ThreadId(0)), "Thread-0");
        assert_eq!(format!("{}", ThreadId(42)), "Thread-42");
    }

    #[test]
    fn park_state_unpark_before_park() {
        let ps = ParkState::new();
        // Unpark first → next park returns immediately
        ps.unpark();
        ps.park(Some(std::time::Duration::from_millis(100)));
        // If park didn't return immediately, the test would timeout
    }

    #[test]
    fn park_state_park_with_timeout() {
        let ps = ParkState::new();
        let start = std::time::Instant::now();
        ps.park(Some(std::time::Duration::from_millis(50)));
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Park should have blocked for ~50ms, got {}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn park_state_unpark_wakes_parked_thread() {
        let ps = Arc::new(ParkState::new());
        let ps2 = ps.clone();
        let handle = std::thread::spawn(move || {
            // This will block until unparked
            ps2.park(Some(std::time::Duration::from_secs(5)));
        });
        // Give the thread time to park
        std::thread::sleep(std::time::Duration::from_millis(20));
        ps.unpark();
        handle.join().unwrap();
    }
}
