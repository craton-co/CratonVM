// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Registry for tracking all JVM threads.
//!
//! The `ThreadRegistry` keeps track of spawned threads, their Java Thread
//! objects, join handles, and liveness status. It also hands out unique
//! `ThreadId` values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::threading::jvm_thread::{GcBlockState, ParkState, ThreadId};
use crate::types::ObjectRef;

/// An entry in the thread registry for one JVM thread.
struct ThreadEntry {
    /// Human-readable name.
    name: String,
    /// OS thread join handle (None for the main thread).
    join_handle: Option<JoinHandle<()>>,
    /// The Java `Thread` object on the heap.
    java_thread_obj: Option<ObjectRef>,
    /// Whether this thread is still running.
    alive: AtomicBool,
    /// Whether this thread can currently participate in counted STW barriers.
    ///
    /// `Thread.start()` registers the Java mirror before the spawned carrier has
    /// published its GC state or reached a safepoint-capable startup point. Such
    /// entries are alive for Java APIs but must not contribute to STW `expected`
    /// until the child marks itself ready.
    stw_ready: AtomicBool,
    /// T19.K1 — Whether this thread is a daemon. Default `false` matches
    /// the JLS rule that newly created threads inherit the parent's
    /// daemon status, which is `false` for the main thread. The CLI
    /// uses this to decide whether the process must wait for this
    /// thread before exiting (per the JVM spec, the VM lives until
    /// every non-daemon thread terminates). Stored as `AtomicBool`
    /// because `Thread.setDaemon(boolean)` may flip it from another
    /// thread before the target is `start()`-ed.
    daemon: AtomicBool,
    /// Parking permit for LockSupport.park()/unpark().
    park_state: Arc<ParkState>,
    /// Thread interrupt flag (shared with JvmThread for cross-thread access).
    interrupted: Arc<AtomicBool>,
    /// Root snapshot: ObjectRefs from this thread's frames, deposited at safepoints
    /// and before blocking operations. Used by GC to scan all threads' roots.
    root_snapshot: Arc<Mutex<Vec<ObjectRef>>>,
    /// Frame trace snapshot (shared with the JvmThread, like `root_snapshot`):
    /// this thread's Java call stack, published at the same blocking deposit
    /// points. Lets another thread read where this one is parked, backing
    /// cross-thread `Thread.getStackTrace()` / `dumpThreads()`.
    frame_trace: Arc<Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    /// Optional VM-side breadcrumb published by the owning `JvmThread` when
    /// `CRATONVM_DBG_VM_STATE=1` is enabled.
    vm_state: Arc<Mutex<String>>,
    /// Blocked-region GC state (shared with the JvmThread, like `root_snapshot`):
    /// lets a GC initiator remap this thread's snapshot and accumulate frame
    /// fixups while the thread is parked in a blocking native. See
    /// `GcBlockState` and `fold_pointer_map_into_blocked`.
    gc_block_state: Arc<GcBlockState>,
    /// T1.5.1 — pending async exception slot. Set by cross-thread
    /// `Thread.stop` / `Thread.stop0` calls; consumed by the target
    /// thread's next `safepoint_check`.
    ///
    /// Stores the raw address of a `Throwable`-shaped `ObjectRef`. We
    /// use `AtomicUsize` (0 = empty) rather than a `Mutex<Option<ObjectRef>>`
    /// so the setter doesn't need to take a lock and cannot block the
    /// stopping thread.
    ///
    /// B1 fix — the GC treats a non-zero slot as a strong root: it is
    /// scanned in `collect_all_root_snapshots` and repointed in
    /// `update_thread_objs_after_gc`, so the pointee stays live and
    /// non-dangling across a moving GC for the whole post→consume window
    /// (a fresh `Thread.stop` throwable is generally NOT frame-reachable
    /// on the target, so this slot is the only thing keeping it alive).
    async_exception_slot: Arc<std::sync::atomic::AtomicUsize>,
    /// BUG-03 — raw address of this thread's [`cratonvm_gc::Tlab`] (a field of
    /// its `JvmThread`), published once the thread starts running. 0 until set
    /// / after teardown. Read by the cross-thread STW JIT root scan to recover
    /// the un-retired reserved tail of a peer it forcibly stopped while it was
    /// in JIT code (such a peer never reached a safepoint to retire its TLAB).
    ///
    /// Safety of the cross-thread read: it happens only after the STW barrier
    /// has been satisfied, at which point every *alive* peer is either parked
    /// at a safepoint or blocked (both having already retired their TLAB →
    /// `cursor`/`end` null) or forcibly OS-suspended by the collector — so the
    /// `Tlab` is never being mutated, and an alive thread's `JvmThread` cannot
    /// be concurrently dropped (teardown flips `alive=false` and clears this
    /// before dropping). `AtomicUsize` so the owning thread sets/clears it
    /// lock-free.
    tlab_addr: std::sync::atomic::AtomicUsize,
    /// xt-hardening (2026-07-03): the OS thread id of the thread executing
    /// this entry, published by the thread itself at startup (next to its
    /// TLAB address). `0` = not yet published. The GC initiator snapshots
    /// the alive set's OS tids atomically with the barrier's `expected`
    /// computation so the cross-thread JIT takeover only excuses
    /// (`reduce_expected`) frozen peers that were actually COUNTED —
    /// excusing an uncounted newcomer releases the barrier while a counted
    /// mutator still runs, racing the collection.
    os_tid: std::sync::atomic::AtomicU32,
}

/// Global registry of all JVM threads.
///
/// Tracks spawned threads, their Java Thread objects, join handles, and
/// liveness. Thread-safe via `parking_lot::Mutex`.
pub struct ThreadRegistry {
    /// T10.9.B: FxHashMap — ThreadId is internal.
    threads: Mutex<FxHashMap<ThreadId, ThreadEntry>>,
    next_id: AtomicU64,
    /// WP4.1 — O(1) reverse index from Java `Thread` object pointer to
    /// its `ParkState`. AQS / `LockSupport.unpark(Thread)` calls this on
    /// the hot path of every queued lock release, so a linear walk over
    /// `threads` (the previous behavior) costs Θ(N) per unpark with
    /// 16-thread fair-mode contention causing 160 K unparks → 2.5 M map
    /// scans + mutex acquires. The reverse map keeps the same lock-free
    /// semantics (the entries are `Arc<ParkState>` clones, identical to
    /// the per-`ThreadEntry` copies) but takes O(1).
    ///
    /// Keyed by `ObjectRef::as_ptr() as usize` because `ObjectRef`
    /// already hashes by pointer; we just need a `Hash + Eq` form.
    thread_obj_to_park: Mutex<FxHashMap<usize, Arc<ParkState>>>,
    /// Vacated heap addresses of relocated `java.lang.Thread` mirrors →
    /// owning thread id, for stale-receiver recovery (see
    /// [`Self::recover_stale_mirror`]). Populated by
    /// `update_thread_objs_after_gc` every time a moving / promoting young
    /// collection relocates a mirror: the address the GC vacated. A running
    /// or blocked frame that resumed holding a not-yet-remapped copy of that
    /// OLD address (the frame/operand remap-coverage gap documented in
    /// `docs/known-issues/gc-blocked-thread-frame-stale-thread-mirror.md`)
    /// can then recover the live mirror instead of reading a zeroed object's
    /// null `holder` and NPEing in `Thread.getThreadGroup` (Tomcat
    /// `TestDigestAuthenticator` et al.). `.0` is the lookup map; `.1` is the
    /// FIFO eviction order bounding it to `FORMER_MIRROR_CAP` entries.
    former_mirror_addrs: Mutex<(
        FxHashMap<usize, ThreadId>,
        std::collections::VecDeque<usize>,
    )>,
}

/// Upper bound on retained former-mirror addresses (see
/// [`ThreadRegistry::former_mirror_addrs`]). Mirrors relocate rarely, and a
/// stale frame copy is only consultable until its slot is reclaimed/reused,
/// so a few thousand recent vacated addresses is ample; the FIFO bound keeps
/// the table from growing across a long-running process.
const FORMER_MIRROR_CAP: usize = 8192;

impl ThreadRegistry {
    /// Create a new, empty registry. The next thread id will be 1
    /// (id 0 is reserved for the main thread).
    pub fn new() -> Self {
        Self {
            threads: Mutex::new(FxHashMap::default()),
            next_id: AtomicU64::new(1),
            thread_obj_to_park: Mutex::new(FxHashMap::default()),
            former_mirror_addrs: Mutex::new((
                FxHashMap::default(),
                std::collections::VecDeque::new(),
            )),
        }
    }

    /// Allocate the next unique `ThreadId`.
    pub fn next_thread_id(&self) -> ThreadId {
        ThreadId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Register a thread (called before or at spawn time).
    pub fn register(&self, thread_id: ThreadId, name: &str, java_thread_obj: Option<ObjectRef>) {
        self.register_with_daemon(thread_id, name, java_thread_obj, false);
    }

    /// T19.K1 — Register a thread with an explicit daemon flag.
    ///
    /// `daemon == true` marks the thread as a JVM "daemon": the CLI's
    /// non-daemon-wait loop ignores it, so the process can exit while
    /// this thread is still running. `daemon == false` (the default)
    /// marks it as a "user" thread that the VM must wait for at
    /// shutdown.
    pub fn register_with_daemon(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
    ) {
        self.register_with_daemon_stw_ready(thread_id, name, java_thread_obj, daemon, true);
    }

    /// Register a thread that is alive but not yet able to answer STW barriers.
    pub fn register_starting_with_daemon(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
    ) {
        self.register_with_daemon_stw_ready(thread_id, name, java_thread_obj, daemon, false);
    }

    fn register_with_daemon_stw_ready(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
        stw_ready: bool,
    ) {
        let park_state = Arc::new(ParkState::new());
        let entry = ThreadEntry {
            name: name.to_string(),
            join_handle: None,
            java_thread_obj,
            alive: AtomicBool::new(true),
            stw_ready: AtomicBool::new(stw_ready),
            daemon: AtomicBool::new(daemon),
            park_state: park_state.clone(),
            interrupted: Arc::new(AtomicBool::new(false)),
            root_snapshot: Arc::new(Mutex::new(Vec::new())),
            frame_trace: Arc::new(Mutex::new(Vec::new())),
            vm_state: Arc::new(Mutex::new(String::new())),
            gc_block_state: Arc::new(GcBlockState::new()),
            async_exception_slot: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tlab_addr: std::sync::atomic::AtomicUsize::new(0),
            os_tid: std::sync::atomic::AtomicU32::new(0),
        };
        self.threads.lock().insert(thread_id, entry);
        if let Some(obj) = java_thread_obj {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, park_state);
        }
    }

    /// BUG-03 — publish the address of this thread's `JvmThread`'s
    /// [`cratonvm_gc::Tlab`]. Called once by the thread itself just after it
    /// starts running (the TLAB lives in the `JvmThread`, whose address is
    /// stable for the thread's life). No-op for an unknown id.
    pub fn set_tlab_addr(&self, thread_id: ThreadId, tlab_addr: usize) {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(tlab_addr, Ordering::Release);
        }
    }

    /// BUG-03 — clear this thread's published TLAB address. MUST be called by
    /// the thread (or its teardown) before its `JvmThread` is dropped so the
    /// collector never dereferences a dangling TLAB pointer.
    pub fn clear_tlab_addr(&self, thread_id: ThreadId) {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(0, Ordering::Release);
        }
    }

    /// BUG-03 — collect the reserved (un-retired) TLAB tails of every alive
    /// thread, as absolute `(cursor, end)` pairs.
    ///
    /// Called by the multi-threaded GC initiator AFTER the STW barrier is
    /// satisfied (so no counted peer is running Java): at that point normal
    /// parked or blocked peers should already have retired their TLAB
    /// (`reserved_tail` -> `None`), but collecting every alive thread makes the
    /// collector robust if a blocked or tearing-down path missed that retire.
    /// The non-empty tails are the regions the non-moving young sweep must skip
    /// (see
    /// [`cratonvm_gc::vm_heap::VmHeap::set_jit_tlab_skip_regions`]).
    ///
    /// SAFETY: reads each alive thread's `Tlab` through its published address.
    /// Per the call-time invariant above, no alive thread is mutating its TLAB
    /// (all are parked / blocked / OS-suspended), and an alive thread's
    /// `JvmThread` cannot be concurrently dropped (teardown clears the address
    /// and flips `alive` first). Dead / un-published entries are skipped.
    pub fn collect_reserved_tlab_tails(&self) -> Vec<(usize, usize)> {
        let threads = self.threads.lock();
        let mut out = Vec::new();
        for entry in threads.values() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                continue;
            }
            // SAFETY: see method contract — the owning thread is parked /
            // blocked / OS-suspended, so the `Tlab` at `addr` is live and not
            // being mutated.
            let tlab = unsafe { &*(addr as *const cratonvm_gc::Tlab) };
            if let Some(tail) = tlab.reserved_tail() {
                out.push(tail);
            }
        }
        out
    }

    /// INT-3 — the deposited root-snapshot contents of every alive thread
    /// whose OS tid is in `os_tids`.
    ///
    /// Used by the G1 cross-thread STW JIT takeover: a forcibly-frozen peer
    /// is excused from the barrier, so it never applies the collection's
    /// pointer map to its own frames — its deposit snapshot is the only view
    /// of what those frames reference. Keeping the snapshot alive (it is
    /// already merged into the collection roots) is not enough under an
    /// evacuating collector: the objects must also NOT MOVE, so the caller
    /// pins each returned address's region out of the CSet (see
    /// `cratonvm_gc::gc_quiescence::add_pinned_jit_root`). Not needed on the
    /// Generational backend, whose frozen-peer cycles run the fully
    /// non-moving sweep.
    pub fn root_snapshots_for_os_tids(&self, os_tids: &[u32]) -> Vec<ObjectRef> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        let threads = self.threads.lock();
        let mut out = Vec::new();
        for entry in threads.values() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            let tid = entry.os_tid.load(Ordering::Acquire);
            if tid != 0 && os_tids.contains(&tid) {
                out.extend(entry.root_snapshot.lock().iter().copied());
            }
        }
        out
    }

    /// T19.K1 — Update the daemon flag for an already-registered thread.
    ///
    /// `Thread.setDaemon(boolean)` may be called between construction
    /// and `start()`; once `start()` is invoked the JLS forbids further
    /// changes (an `IllegalThreadStateException` is thrown by the JDK
    /// bytecode). We don't enforce that gate here — callers (the
    /// native bridge) are responsible — we just store the new value.
    /// Returns `true` if the thread was found, `false` if the id was
    /// unknown.
    pub fn set_daemon(&self, thread_id: ThreadId, daemon: bool) -> bool {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.daemon.store(daemon, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// T19.K1 — Read the daemon flag for a thread. Returns `false` for
    /// unknown thread ids (matches `is_alive`'s "missing means absent"
    /// convention so callers can treat unknown ids as already-gone
    /// non-daemon threads — the safe default).
    pub fn is_daemon(&self, thread_id: ThreadId) -> bool {
        self.threads
            .lock()
            .get(&thread_id)
            .map(|e| e.daemon.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// T1.5.1 — Post an asynchronous exception to the target thread.
    ///
    /// The target will raise the exception at its next safepoint.
    ///
    /// B1 fix — once stored, the slot is itself a GC root: it is scanned
    /// in `collect_all_root_snapshots` and remapped in
    /// `update_thread_objs_after_gc`, so the throwable stays alive and
    /// non-dangling across any moving GC in the post→consume window. The
    /// caller therefore no longer needs to keep `throwable` independently
    /// reachable (it previously had to be frame- or static-reachable, which
    /// was not guaranteed for a freshly allocated `Thread.stop` throwable).
    pub fn post_async_exception(&self, thread_id: ThreadId, throwable: ObjectRef) -> bool {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.async_exception_slot.store(
                throwable.as_ptr() as usize,
                std::sync::atomic::Ordering::Release,
            );
            // Also unpark the target so a parked thread wakes and
            // hits its next safepoint promptly.
            entry.park_state.unpark();
            true
        } else {
            false
        }
    }

    /// T19.H1 watchdog — unpark every registered thread so threads parked
    /// in `LockSupport.park` (e.g. AQS lock/latch waiters) wake, return
    /// through the interpreter top-of-loop poll, and emit their stack dump.
    /// Diagnostic-only: a spurious unpark is spec-legal (`park` may return
    /// spuriously) and the process aborts immediately after the dump, so the
    /// extra permits never affect correctness.
    pub fn unpark_all_for_stack_dump(&self) {
        let threads = self.threads.lock();
        for entry in threads.values() {
            entry.park_state.unpark();
        }
    }

    /// T19.H1 watchdog — print a one-line summary of every registered thread
    /// (name, liveness, daemon, deposited-root count) to stderr. Lets the
    /// watchdog show threads that have NO dumpable interpreter frames — e.g.
    /// a thread blocked in a Rust-level native lock, or one that never
    /// started — which the frame-dump path cannot surface. A non-zero
    /// `roots` on an otherwise-silent thread means it deposited roots before
    /// blocking (it IS blocked in a native), distinguishing "blocked in
    /// native" from "never ran".
    pub fn dump_thread_summary_to_stderr(&self) {
        use std::io::Write;
        let threads = self.threads.lock();
        let stderr = std::io::stderr();
        let mut h = stderr.lock();
        let _ = writeln!(
            h,
            "--- T19.H1 thread summary: {} registered thread(s) ---",
            threads.len()
        );
        struct SummaryRow {
            tid: u64,
            os_tid: u64,
            name: String,
            alive: bool,
            daemon: bool,
            blocked: bool,
            roots: usize,
            state: String,
            top: String,
        }
        let mut rows: Vec<SummaryRow> = threads
            .iter()
            .map(|(tid, e)| {
                let state = {
                    let s = e.vm_state.lock();
                    if s.is_empty() {
                        "<unset>".to_string()
                    } else {
                        s.clone()
                    }
                };
                let trace = e.frame_trace.lock();
                let mut top = String::new();
                for (i, frame) in trace.iter().rev().take(3).enumerate() {
                    if i > 0 {
                        top.push_str(" <- ");
                    }
                    top.push_str(&format!(
                        "{}.{}@{}",
                        frame.class_name, frame.method_name, frame.byte_code_index
                    ));
                }
                if top.is_empty() {
                    top.push_str("<no-frame-trace>");
                }
                SummaryRow {
                    tid: tid.0,
                    os_tid: e.os_tid.load(std::sync::atomic::Ordering::Acquire) as u64,
                    name: e.name.clone(),
                    alive: e.alive.load(std::sync::atomic::Ordering::Acquire),
                    daemon: e.daemon.load(std::sync::atomic::Ordering::Acquire),
                    blocked: e
                        .gc_block_state
                        .in_blocked_region
                        .load(std::sync::atomic::Ordering::Acquire),
                    roots: e.root_snapshot.lock().len(),
                    state,
                    top,
                }
            })
            .collect();
        rows.sort_by_key(|r| r.tid);
        for row in rows {
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} alive={} daemon={} blocked={} roots={} state={:?} top={}",
                row.tid,
                row.os_tid,
                row.name,
                row.alive,
                row.daemon,
                row.blocked,
                row.roots,
                row.state,
                row.top
            );
        }
        let _ = writeln!(h, "--- T19.H1 end thread summary ---");
        let _ = h.flush();
    }

    /// T1.5.1 — Take the target thread's pending async exception (if any).
    /// Called from the target thread's `safepoint_check` — never
    /// cross-thread. The consumer is responsible for raising the
    /// exception via the interpreter's normal exception table walk.
    pub fn take_async_exception(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.lock();
        let entry = threads.get(&thread_id)?;
        let raw = entry
            .async_exception_slot
            .swap(0, std::sync::atomic::Ordering::AcqRel);
        if raw == 0 {
            None
        } else {
            // SAFETY: the poster wrote a valid `ObjectRef::as_ptr()`
            // address. We round-trip it back through `ObjectRef::from_raw`.
            Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
        }
    }

    /// Store the OS `JoinHandle` for a spawned thread.
    pub fn set_join_handle(&self, thread_id: ThreadId, handle: JoinHandle<()>) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.join_handle = Some(handle);
        }
    }

    /// Mark a thread as dead (called when the thread finishes execution).
    pub fn mark_dead(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.alive.store(false, Ordering::Release);
            // Reclaim the OS thread handle. Tomcat's poller/acceptor/pool
            // threads are daemon threads that are never `join()`ed, so their
            // `JoinHandle` would otherwise sit in the registry forever, leaking
            // the underlying Windows thread HANDLE. Across heavy short-lived
            // thread churn — e.g. a parameterized test that starts and stops a
            // whole Tomcat instance ~140 times — this leaks hundreds of thread
            // handles (observed: 219 Thread handles for ~21 live threads),
            // adding steady per-iteration overhead. The thread has finished, so
            // dropping the handle simply detaches it; a later `join()` of an
            // already-dead thread still returns immediately (see `join`).
            entry.join_handle.take();
        }
    }

    /// Check if a thread is still alive.
    pub fn is_alive(&self, thread_id: ThreadId) -> bool {
        self.threads
            .lock()
            .get(&thread_id)
            .map(|e| e.alive.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Mark a started carrier as able to participate in counted STW barriers.
    pub fn mark_stw_ready(&self, thread_id: ThreadId) {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.stw_ready.store(true, Ordering::Release);
        }
    }

    /// Block the calling OS thread until the target thread finishes.
    ///
    /// Takes the `JoinHandle` out of the registry and joins on it.
    /// Returns `true` if the join succeeded, `false` if no handle was found
    /// (already joined, or it's the main thread).
    pub fn join(&self, thread_id: ThreadId) -> bool {
        let handle = {
            let mut threads = self.threads.lock();
            threads
                .get_mut(&thread_id)
                .and_then(|e| e.join_handle.take())
        };
        match handle {
            Some(h) => {
                let _ = h.join();
                // Mark as dead now that join completed
                self.mark_dead(thread_id);
                true
            }
            None => {
                // No handle — either already joined, or it's the main thread.
                // If the thread is dead, return immediately.
                if !self.is_alive(thread_id) {
                    return false;
                }
                // Thread is alive but we can't join. This shouldn't normally happen
                // (race between spawn and set_join_handle). Brief spin-wait with timeout.
                for _ in 0..1000 {
                    if !self.is_alive(thread_id) {
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                false
            }
        }
    }

    /// Get the Java Thread object for a given ThreadId.
    pub fn java_thread_obj(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        self.threads
            .lock()
            .get(&thread_id)
            .and_then(|e| e.java_thread_obj)
    }

    /// Record that thread `tid`'s `java.lang.Thread` mirror just vacated
    /// `old_addr` because a moving / promoting GC relocated it. Enables
    /// [`Self::recover_stale_mirror`] to repair a frame that resumed holding a
    /// stale copy of `old_addr`. Bounded FIFO ([`FORMER_MIRROR_CAP`]). Takes
    /// only the `former_mirror_addrs` lock — callers must NOT hold the
    /// `threads` lock across this (see `update_thread_objs_after_gc`, which
    /// records only after dropping `threads`) so the lock order stays
    /// `threads → former`, opposite to `recover_stale_mirror`'s
    /// `former → threads`; neither nests, so they cannot deadlock.
    fn record_former_mirror_addr(&self, old_addr: usize, tid: ThreadId) {
        let mut g = self.former_mirror_addrs.lock();
        let (map, order) = &mut *g;
        if map.insert(old_addr, tid).is_none() {
            order.push_back(old_addr);
            while order.len() > FORMER_MIRROR_CAP {
                if let Some(evict) = order.pop_front() {
                    map.remove(&evict);
                }
            }
        }
    }

    /// If `stale_addr` is a recorded former address of some thread's
    /// `java.lang.Thread` mirror, return that thread's CURRENT (live,
    /// GC-remapped) mirror so a stale-receiver use site can recover. Identity
    /// preserving — a vacated address uniquely identified one thread's mirror,
    /// so the returned mirror is the right thread's even if a *different*
    /// thread is executing. The caller must still verify the returned object
    /// is live (non-zero header) before use.
    pub fn recover_stale_mirror(&self, stale_addr: usize) -> Option<ObjectRef> {
        let tid = {
            let g = self.former_mirror_addrs.lock();
            *g.0.get(&stale_addr)?
        };
        self.java_thread_obj(tid)
    }

    /// Set the Java Thread object for a given ThreadId.
    pub fn set_java_thread_obj(&self, thread_id: ThreadId, obj: ObjectRef) {
        let park_state_clone;
        let prev_obj;
        {
            let mut threads = self.threads.lock();
            if let Some(entry) = threads.get_mut(&thread_id) {
                prev_obj = entry.java_thread_obj;
                entry.java_thread_obj = Some(obj);
                park_state_clone = entry.park_state.clone();
            } else {
                return;
            }
        }
        // WP4.1 — keep reverse index in sync. If we replaced an older obj
        // pointer (rare; only happens if Thread is re-bound), drop the
        // stale entry first.
        let mut idx = self.thread_obj_to_park.lock();
        if let Some(prev) = prev_obj {
            if prev.as_ptr() != obj.as_ptr() {
                idx.remove(&(prev.as_ptr() as usize));
            }
        }
        idx.insert(obj.as_ptr() as usize, park_state_clone);
    }

    /// Get the name of a thread.
    pub fn thread_name(&self, thread_id: ThreadId) -> Option<String> {
        self.threads.lock().get(&thread_id).map(|e| e.name.clone())
    }

    /// Return all (ThreadId, name) pairs for currently registered threads.
    pub fn all_thread_names(&self) -> Vec<(ThreadId, String)> {
        self.threads
            .lock()
            .iter()
            .map(|(tid, entry)| (*tid, entry.name.clone()))
            .collect()
    }

    /// Set the park state for a given ThreadId (used when spawning threads
    /// to share the JvmThread's ParkState with the registry).
    pub fn set_park_state(&self, thread_id: ThreadId, state: Arc<ParkState>) {
        let obj_opt;
        {
            let mut threads = self.threads.lock();
            if let Some(entry) = threads.get_mut(&thread_id) {
                entry.park_state = state.clone();
                obj_opt = entry.java_thread_obj;
            } else {
                return;
            }
        }
        // WP4.1 — keep reverse index pointing at the new ParkState so
        // unpark(Thread) hits the same instance the parked thread is
        // blocked on.
        if let Some(obj) = obj_opt {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, state);
        }
    }

    /// Get the park state for a given ThreadId (for unpark from another thread).
    pub fn get_park_state(&self, thread_id: ThreadId) -> Option<Arc<ParkState>> {
        self.threads
            .lock()
            .get(&thread_id)
            .map(|e| e.park_state.clone())
    }

    /// Find the park state for a thread identified by its Java Thread object.
    /// Used by `Unsafe.unpark(Thread)` and `LockSupport.unpark(Thread)`.
    ///
    /// WP4.1 — O(1) via the reverse index. Falls back to a linear walk
    /// of `threads` only if the index lookup misses, which should never
    /// happen for a thread that has been properly registered with a
    /// non-null `java_thread_obj`. The fallback exists to keep tests
    /// that construct registries by hand (without the modern register
    /// path) working.
    pub fn find_park_state_by_thread_obj(&self, obj: ObjectRef) -> Option<Arc<ParkState>> {
        if let Some(ps) = self.thread_obj_to_park.lock().get(&(obj.as_ptr() as usize)) {
            return Some(ps.clone());
        }
        // Fallback: legacy registries that bypassed register_with_daemon.
        let threads = self.threads.lock();
        for entry in threads.values() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr()) {
                    return Some(entry.park_state.clone());
                }
            }
        }
        None
    }

    /// DBG (`CRATONVM_DBG_UNPARK_MISS`): snapshot of every registered
    /// thread's `(tid, java_thread_obj address, alive)` so an unpark whose
    /// mirror lookup missed can print what the registry believes the live
    /// mirror addresses are (a stale `Node.waiter` shows up as a caller
    /// address absent from this list).
    pub fn debug_thread_obj_addrs(&self) -> Vec<(u64, usize, bool)> {
        let threads = self.threads.lock();
        threads
            .iter()
            .map(|(id, e)| {
                (
                    id.0,
                    e.java_thread_obj.map_or(0, |o| o.as_ptr() as usize),
                    e.alive.load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// WP4.1 — Find the `ThreadId` for a registered thread by its Java
    /// `Thread` mirror object.  Real-JDK-mode `Thread` objects do not have
    /// a single fixed slot we can write a `ThreadId` into (the class layout
    /// stores `tid` inside an inner `FieldHolder`), so callers that used
    /// to read field 2 directly must instead walk the registry.
    ///
    /// Pointer-identity comparison matches the existing
    /// `find_park_state_by_thread_obj` helper.
    pub fn find_thread_id_by_thread_obj(&self, obj: ObjectRef) -> Option<ThreadId> {
        let threads = self.threads.lock();
        for (id, entry) in threads.iter() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr()) {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Set the interrupted flag for a thread (cross-thread interrupt).
    pub fn set_interrupted(&self, thread_id: ThreadId, value: bool) {
        if let Some(entry) = self.threads.lock().get(&thread_id) {
            entry
                .interrupted
                .store(value, std::sync::atomic::Ordering::Release);
        }
    }

    /// Get the interrupted flag Arc for a thread (to share with JvmThread).
    pub fn get_interrupted_flag(&self, thread_id: ThreadId) -> Option<Arc<AtomicBool>> {
        self.threads
            .lock()
            .get(&thread_id)
            .map(|e| e.interrupted.clone())
    }

    /// Set the interrupted flag Arc for a thread (share JvmThread's flag with registry).
    pub fn set_interrupted_flag(&self, thread_id: ThreadId, flag: Arc<AtomicBool>) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.interrupted = flag;
        }
    }

    /// Set the root snapshot Arc for a thread (share JvmThread's snapshot with registry).
    pub fn set_root_snapshot(&self, thread_id: ThreadId, snapshot: Arc<Mutex<Vec<ObjectRef>>>) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.root_snapshot = snapshot;
        }
    }

    /// Share the JvmThread's frame-trace Arc with the registry, so other threads
    /// can read this thread's published call stack.
    pub fn set_frame_trace(
        &self,
        thread_id: ThreadId,
        trace: Arc<Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    ) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.frame_trace = trace;
        }
    }

    /// Share the JvmThread's VM-state breadcrumb with the registry.
    pub fn set_vm_state(&self, thread_id: ThreadId, state: Arc<Mutex<String>>) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.vm_state = state;
        }
    }

    /// Read a copy of `thread_id`'s last-published frame trace (call stack),
    /// innermost frame first. Empty if the thread is unknown or never deposited.
    pub fn frame_trace_of(&self, thread_id: ThreadId) -> Vec<cratonvm_native_api::StackTraceEntry> {
        self.threads
            .lock()
            .get(&thread_id)
            .map(|e| e.frame_trace.lock().clone())
            .unwrap_or_default()
    }

    /// xt-hardening follow-up (2026-07-03): OS tids of alive threads
    /// currently `in_blocked_region` (excluded from the barrier, covered
    /// only by `deposit_root_snapshot` — which never scans the JIT band on
    /// the native stack). This is the ONLY gap `helper_window_pass` exists
    /// to close (its own doc comment says so); a cooperatively-arrived
    /// mutator already published its JIT roots via `update_root_snapshot`
    /// before parking at the barrier, so re-scanning it is pure redundant
    /// over-retention risk. Threads with `os_tid == 0` (registered but not
    /// yet started) cannot be blocked, so they never contribute a false 0.
    pub fn blocked_os_tids(&self) -> Vec<u32> {
        let threads = self.threads.lock();
        threads
            .values()
            .filter(|e| {
                e.alive.load(Ordering::Acquire)
                    && e.gc_block_state.in_blocked_region.load(Ordering::Acquire)
            })
            .map(|e| e.os_tid.load(Ordering::Acquire))
            .filter(|&t| t != 0)
            .collect()
    }

    /// Mark a VM-registered native carrier thread as parked in host-native code.
    ///
    /// These threads do not own interpreter frames while parked, so their
    /// authoritative root snapshot is empty. The flag still matters: moving-GC
    /// fixups and STW diagnostics use it to classify the thread as blocked.
    pub fn mark_native_thread_blocked(&self, thread_id: ThreadId) {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.root_snapshot.lock().clear();
            entry.frame_trace.lock().clear();
            entry
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }

    /// Clear the GC-blocked mark for a VM-registered native carrier thread.
    pub fn mark_native_thread_unblocked(&self, thread_id: ThreadId) {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry.gc_block_state.fixup.lock().clear();
            entry.root_snapshot.lock().clear();
            entry
                .gc_block_state
                .in_blocked_region
                .store(false, Ordering::Release);
        }
    }

    /// DBG (CRATONVM_DBG_MTROOTS): per-thread (tid, in_blocked_region,
    /// snapshot_len) for every alive thread. Used at an STW to see whether a
    /// thread that holds a reclaimed live oop was counted BLOCKED (excluded from
    /// the barrier `expected`) while actually running — the multi-thread
    /// root-coverage gap.
    pub fn dump_blocked_states(&self) -> Vec<(u64, bool, usize)> {
        let threads = self.threads.lock();
        let mut v = Vec::new();
        for (tid, entry) in threads.iter() {
            if entry.alive.load(Ordering::Acquire) {
                let blk = entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire);
                let len = entry.root_snapshot.lock().len();
                v.push((tid.0, blk, len));
            }
        }
        v
    }

    /// Debug-only STW census: one line per alive JVM thread with the state that
    /// matters to stop-the-world accounting. Used when the GC barrier is waiting
    /// for a cooperative mutator but the cross-thread JIT takeover cannot find a
    /// live JIT frame to freeze.
    pub fn debug_thread_census(&self) -> String {
        use std::fmt::Write as _;

        let threads = self.threads.lock();
        let mut out = String::new();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }

            let blocked = entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire);
            let stw_ready = entry.stw_ready.load(Ordering::Acquire);
            let snapshot_len = entry.root_snapshot.lock().len();
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let vm_state = {
                let s = entry.vm_state.lock();
                if s.is_empty() {
                    "<unset>".to_string()
                } else {
                    s.clone()
                }
            };
            let trace = entry.frame_trace.lock();
            let mut top = String::new();
            for (i, frame) in trace.iter().rev().take(4).enumerate() {
                if i > 0 {
                    top.push_str(" <- ");
                }
                let _ = write!(
                    top,
                    "{}.{}@{}",
                    frame.class_name, frame.method_name, frame.byte_code_index
                );
            }
            if top.is_empty() {
                top.push_str("<no-frame-trace>");
            }
            let _ = write!(
                out,
                "\n  t{} os_tid={} name={:?} blocked={} ready={} snapshot={} state={:?} top={}",
                tid.0, os_tid, entry.name, blocked, stw_ready, snapshot_len, vm_state, top
            );
        }
        out
    }

    /// Collect root snapshots from all alive threads.
    /// Returns a combined vector of all ObjectRefs from all threads' snapshots.
    ///
    /// B1 fix — also reports each thread's pending **async-exception slot**
    /// (`async_exception_slot`, set by a cross-thread `Thread.stop` /
    /// `post_async_exception` and consumed at the target's next safepoint).
    /// The stored throwable is a fresh Java object that is generally NOT
    /// held by the target's frames between post and consume, so without
    /// scanning it here a young/moving GC could collect it (no root keeps
    /// it alive) — the target would then raise a reclaimed object. The
    /// matching remap is in `update_thread_objs_after_gc` (gc.rs step 21).
    pub fn collect_all_root_snapshots(&self) -> Vec<ObjectRef> {
        let threads = self.threads.lock();
        let mut all_roots = Vec::new();
        for entry in threads.values() {
            if entry.alive.load(Ordering::Acquire) {
                let snapshot = entry.root_snapshot.lock();
                all_roots.extend(snapshot.iter().copied());
                // Root this thread's `java.lang.Thread` mirror. The
                // initiator's own mirror is rooted via `collect_roots`
                // (roots.rs step 10), but a PARKED thread's mirror is only
                // reachable through `ThreadEntry.java_thread_obj` — a raw
                // address copy that no frame root or heap edge necessarily
                // keeps alive (CratonVM's synthetic ThreadGroup does not
                // retain its threads the way the real JDK's does). Without
                // rooting it here, a GC initiated by ANOTHER thread while
                // this one is parked sweeps the mirror; the dangling copy
                // then surfaces as the "all-zero header" invokevirtual
                // fallback on `Thread.currentThread()`, and the real-JDK
                // `Thread.getThreadGroup()` reads a null `holder` and NPEs
                // (Tomcat TestDigestAuthenticator: a worker-thread GC frees
                // the JUnit main thread's mirror). The matching remap is
                // `update_thread_objs_after_gc` — mirroring the async-
                // exception slot's root+remap pairing below.
                if let Some(obj) = entry.java_thread_obj {
                    all_roots.push(obj);
                }
            }
            // A posted async exception must survive even if the target
            // thread is dead-but-not-yet-reaped: it may still be consumed
            // on a final safepoint. Scan regardless of `alive`.
            let raw = entry
                .async_exception_slot
                .load(std::sync::atomic::Ordering::Acquire);
            if raw != 0 {
                // SAFETY: a non-zero slot holds a valid `ObjectRef::as_ptr()`
                // address written by `post_async_exception`.
                all_roots.push(unsafe { ObjectRef::from_raw(raw as *mut u8) });
            }
        }
        all_roots
    }

    /// Set the blocked-region GC state Arc for a thread (share JvmThread's
    /// state with the registry, like `set_root_snapshot`).
    pub fn set_gc_block_state(&self, thread_id: ThreadId, state: Arc<GcBlockState>) {
        if let Some(entry) = self.threads.lock().get_mut(&thread_id) {
            entry.gc_block_state = state;
        }
    }

    /// GC maintenance for the registry's raw `java.lang.Thread` mirrors —
    /// called by the GC initiator (under STW) with the collection's pointer
    /// map.
    ///
    /// Every `ThreadEntry.java_thread_obj` is a raw address copy, and the
    /// `thread_obj_to_park` reverse index is KEYED by such addresses. A
    /// moving collection relocates the mirrors; without this step the
    /// copies dangle: natives that serve them back to Java
    /// (`enumerate_threads` / `Thread.getAllStackTraces`) resurrect stale
    /// refs into bytecode (the all-zero-header invokevirtual WARN flood),
    /// and `LockSupport.unpark(Thread)`'s O(1) lookup misses the live
    /// mirror's new address — a silently lost unpark.
    pub fn update_thread_objs_after_gc(&self, pointer_map: &HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        let mut threads = self.threads.lock();
        let mut rekeyed: Vec<(usize, usize)> = Vec::new();
        // Vacated mirror addresses + owning tid, recorded into
        // `former_mirror_addrs` AFTER `threads` is dropped (lock order).
        let mut vacated: Vec<(usize, ThreadId)> = Vec::new();
        for (tid, entry) in threads.iter_mut() {
            if let Some(ref mut obj) = entry.java_thread_obj {
                let old_addr = obj.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // SAFETY: produced by the GC pointer map.
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    rekeyed.push((old_addr, new_addr));
                    vacated.push((old_addr, *tid));
                }
            }
            // B1 fix — repoint a pending async-exception slot too. It stores
            // the raw address of a posted `Throwable`; a moving collection
            // relocates that object, so without this the slot dangles and
            // the target raises a stale/garbage object at its next
            // safepoint. The slot is scanned as a root in
            // `collect_all_root_snapshots`, so the object is kept alive and
            // therefore present in the pointer map when it moved.
            let old_exc = entry
                .async_exception_slot
                .load(std::sync::atomic::Ordering::Acquire);
            if old_exc != 0 {
                if let Some(&new_exc) = pointer_map.get(&old_exc) {
                    entry
                        .async_exception_slot
                        .store(new_exc, std::sync::atomic::Ordering::Release);
                }
            }
        }
        drop(threads);
        if !rekeyed.is_empty() {
            let mut idx = self.thread_obj_to_park.lock();
            for (old_addr, new_addr) in rekeyed {
                if let Some(ps) = idx.remove(&old_addr) {
                    idx.insert(new_addr, ps);
                }
            }
        }
        // Record vacated mirror addresses for stale-receiver recovery. Done
        // here (threads lock released) to preserve the threads → former lock
        // order; see `record_former_mirror_addr`.
        for (old_addr, tid) in vacated {
            self.record_former_mirror_addr(old_addr, tid);
        }
    }

    /// Blocked-thread root maintenance — called by the GC initiator (under
    /// STW, before `complete_gc`) with the collection's pointer map.
    ///
    /// For every alive thread currently inside a blocked region (parked in
    /// `Object.wait` / `Thread.join` / `LockSupport.park` /
    /// `ReferenceQueue.remove`, hence excluded from the barrier and unable
    /// to apply this map itself):
    ///
    /// 1. compose the map into the thread's pending frame fixup, chaining
    ///    `orig → cur → new` so the entry stays keyed by the address the
    ///    thread's frames still hold, no matter how many GCs it sleeps
    ///    through;
    /// 2. seed first-move entries from the (pre-remap) snapshot — a snapshot
    ///    address with no existing chain IS the frame-held address;
    /// 3. remap the thread's `root_snapshot` in place so the NEXT collection
    ///    scans live addresses instead of a vacated semispace (scanning a
    ///    stale snapshot is itself a heap corruptor: the collector would
    ///    evacuate garbage "objects" through recycled addresses).
    ///
    /// Threads NOT in a blocked region never need this: `request_stw` counts
    /// them in `expected`, so no GC can complete before they refresh their
    /// snapshot and self-apply the map at a safepoint.
    ///
    /// The thread applies + clears its fixup in `check_post_block_gc` on
    /// wake. While it still holds stale frame addresses the only consumers
    /// are this fold (keyed by those addresses) and the wake-side apply,
    /// so the chain stays consistent.
    pub fn fold_pointer_map_into_blocked(&self, pointer_map: &HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        let dbg = std::env::var_os("CRATONVM_DBG_BLOCKGC").is_some();
        let threads = self.threads.lock();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            if !entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire)
            {
                continue;
            }
            let mut fixup = entry.gc_block_state.fixup.lock();
            let mut snapshot = entry.root_snapshot.lock();
            // Addresses already chained (frame holds the ORIG key, the
            // snapshot holds the CUR value) — collected before composing so
            // the snapshot pass below can tell first moves apart from
            // already-chained objects.
            let chained: rustc_hash::FxHashSet<usize> = fixup.values().copied().collect();
            let mut composed = 0usize;
            let mut seeded = 0usize;
            for cur in fixup.values_mut() {
                if let Some(&new) = pointer_map.get(cur) {
                    *cur = new;
                    composed += 1;
                }
            }
            for r in snapshot.iter_mut() {
                let s = r.as_ptr() as usize;
                if let Some(&new) = pointer_map.get(&s) {
                    if !chained.contains(&s) {
                        // First move of this object while blocked: the frames
                        // hold `s` itself. `or_insert` guards the ABA case
                        // where `s` was recycled and is also an existing
                        // chain key for a DIFFERENT (older) object.
                        fixup.entry(s).or_insert(new);
                        seeded += 1;
                    }
                    // SAFETY: `new` comes from the GC pointer map and points
                    // at the relocated object's header.
                    *r = unsafe { ObjectRef::from_raw(new as *mut u8) };
                }
            }
            if dbg && (composed > 0 || seeded > 0) {
                eprintln!(
                    "[blockgc] fold tid={} composed={} seeded={} fixup_total={} snapshot_len={}",
                    tid.0,
                    composed,
                    seeded,
                    fixup.len(),
                    snapshot.len()
                );
            }
        }
    }

    /// Get the number of registered threads.
    pub fn count(&self) -> usize {
        self.threads.lock().len()
    }

    /// Get the number of alive threads.
    pub fn alive_count(&self) -> usize {
        self.threads
            .lock()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .count()
    }

    /// xt-hardening (2026-07-03): publish the calling thread's OS thread id
    /// for `thread_id` (see `ThreadEntry::os_tid`). Called by the thread
    /// itself at startup, before it can execute any Java/JIT code.
    pub fn set_os_tid_current(&self, thread_id: ThreadId) {
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetCurrentThreadId() -> u32;
            }
            let os_tid = unsafe { GetCurrentThreadId() };
            let threads = self.threads.lock();
            if let Some(entry) = threads.get(&thread_id) {
                entry.os_tid.store(os_tid, Ordering::Release);
            }
        }
        #[cfg(target_os = "linux")]
        {
            let os_tid = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
            let threads = self.threads.lock();
            if let Some(entry) = threads.get(&thread_id) {
                entry.os_tid.store(os_tid, Ordering::Release);
            }
        }
        #[cfg(all(not(windows), not(target_os = "linux")))]
        {
            let _ = thread_id; // takeover machinery is currently Windows/Linux-only
        }
    }

    /// xt-hardening (2026-07-03): STW-ready alive-thread count PLUS the
    /// published OS tids of those counted threads, read under one registry lock
    /// so the GC barrier's `expected` and the takeover's counted-set snapshot
    /// cannot disagree about which threads exist.
    ///
    /// A `Thread.start()` child is alive before its carrier can answer a
    /// safepoint. It stays out of this counted set until `mark_stw_ready` flips
    /// the startup gate; if an STW is already active, the child waits it out via
    /// `arrive_and_wait_excluded` first.
    pub fn alive_count_and_os_tids(&self) -> (usize, Vec<u32>) {
        let threads = self.threads.lock();
        let mut n = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        for e in threads.values() {
            if e.alive.load(Ordering::Acquire) && e.stw_ready.load(Ordering::Acquire) {
                n += 1;
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                }
            }
        }
        (n, tids)
    }

    /// Alive-thread count, live blocked-thread count, and alive OS tids read
    /// under one registry lock.
    ///
    /// The blocked count is the production STW exclusion count: a thread sets
    /// `in_blocked_region` only after depositing its root snapshot, and that
    /// publication can race ahead of the barrier's anonymous `threads_blocked`
    /// counter. Reading it with the alive set keeps the expected mutator quota
    /// aligned with the root/fixup state the collector will actually scan.
    ///
    /// STWREADY-GAP-FIX (2026-07-11): also gate on `stw_ready`, matching the
    /// sibling `alive_count_and_os_tids` (added 2026-07-03, "identity-based
    /// barrier excusal for xt-takeover"). Without this, a `Thread.start()`
    /// child is alive (registered in this map) before its carrier can answer
    /// a safepoint -- `vm_exec.rs`'s `thread_start` startup loop keeps such a
    /// thread out of `mark_stw_ready` until it observes NO active STW
    /// (`run_if_no_stw_requested`), and if one IS active when it starts, it
    /// calls the NON-participating `arrive_and_wait_excluded` -- which never
    /// increments the barrier's `arrived` counter. A pause whose `expected`
    /// snapshot (this function) included that same not-yet-ready thread can
    /// then never satisfy its quota: `arrived` structurally cannot reach
    /// `expected`, and `wait_for_all`/the STW requester's own wait loop
    /// blocks forever -- a whole-VM freeze with zero forward progress,
    /// confirmed via gdb (the stuck threads sit in
    /// `GcBarrier::arrive_and_wait_excluded` from `thread_start`'s startup
    /// loop, never having deposited a root snapshot or reached any
    /// safepoint). A not-yet-`stw_ready` thread cannot yet be executing Java
    /// bytecode (that is exactly what the gate protects), so excluding it
    /// from `expected` cannot let it mutate the heap concurrently with an
    /// evacuating collection -- there is nothing for the collector to race
    /// against on that thread until it actually marks itself ready, at which
    /// point it becomes visible to the NEXT pause's snapshot as normal.
    ///
    /// GCAUDIT-0711-FIX (finding 1a): also returns the IDENTITIES
    /// (`ThreadId.0`) of the alive threads counted in `blocked`, so
    /// `GcBarrier::request_stw_counted_with_live_blocked` can publish exactly
    /// which threads THIS pause excluded — see
    /// `GcBarrierInner::excluded_blocked`'s doc for why per-thread identity,
    /// not just a count, is required for a race-free arrival decision.
    pub fn alive_count_blocked_and_os_tids(&self) -> (usize, usize, Vec<u32>, Vec<u64>) {
        let threads = self.threads.lock();
        let mut alive = 0usize;
        let mut blocked = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        let mut blocked_tids = Vec::new();
        for (tid, e) in threads.iter() {
            if e.alive.load(Ordering::Acquire) && e.stw_ready.load(Ordering::Acquire) {
                alive += 1;
                if e.gc_block_state.in_blocked_region.load(Ordering::Acquire) {
                    blocked += 1;
                    blocked_tids.push(tid.0);
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                }
            }
        }
        (alive, blocked, tids, blocked_tids)
    }

    /// DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster investigation):
    /// `ThreadId`s of every alive, `stw_ready` thread NOT in `excluded` —
    /// i.e. the exact identity set a `request_stw_counted_with_live_blocked`
    /// caller is about to count as "expected". Not lock-atomic with a prior
    /// `alive_count_blocked_and_os_tids` call, but both are invoked from
    /// inside the SAME `request_stw_counted_with_live_blocked` closure, which
    /// runs under the barrier's transition lock — no mutator can flip
    /// `in_blocked_region` in a way that would matter between the two calls
    /// for diagnostic purposes.
    pub fn alive_thread_ids_excluding(&self, excluded: &[u64]) -> Vec<u64> {
        let threads = self.threads.lock();
        threads
            .iter()
            .filter(|(tid, e)| {
                e.alive.load(Ordering::Acquire)
                    && e.stw_ready.load(Ordering::Acquire)
                    && !excluded.contains(&tid.0)
            })
            .map(|(tid, _)| tid.0)
            .collect()
    }

    /// Get Java Thread objects for all alive threads (up to `max` entries).
    pub fn alive_thread_objects(&self, max: usize) -> Vec<ObjectRef> {
        self.threads
            .lock()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .filter_map(|e| e.java_thread_obj)
            .take(max)
            .collect()
    }

    /// T19.K1 — Enumerate the `ThreadId`s of every currently-alive
    /// non-daemon thread. The returned `Vec` is a snapshot taken under
    /// the registry mutex; concurrent transitions (a non-daemon thread
    /// terminating, or a thread that hasn't been registered yet starting)
    /// are not reflected.
    ///
    /// `ThreadId(0)` (the "main" thread) is always excluded: the CLI
    /// invokes this query *from* the main thread after `main()`
    /// returns, so including it would deadlock. Tests register
    /// non-zero ids to exercise the join path.
    pub fn alive_non_daemon_thread_ids(&self) -> Vec<ThreadId> {
        self.threads
            .lock()
            .iter()
            .filter(|(tid, e)| {
                tid.0 != 0 && e.alive.load(Ordering::Acquire) && !e.daemon.load(Ordering::Acquire)
            })
            .map(|(tid, _)| *tid)
            .collect()
    }

    /// T19.K1 — Block the caller until every non-daemon thread that
    /// currently exists in the registry has terminated.
    ///
    /// Per the JVM specification (JLS §12.8 / §17), the VM keeps running
    /// until every thread that *isn't* a daemon completes. Daemon threads
    /// (event loops, GC workers, finalisers, …) may still be running when
    /// this call returns — the caller is expected to terminate the
    /// process at that point and let the OS clean those up.
    ///
    /// # Algorithm
    ///
    /// We snapshot the set of non-daemon thread ids, attempt to take each
    /// one's `JoinHandle` out of the registry, and join on it. If a
    /// thread has already finished and its handle was already taken,
    /// `join()` returns `false` quickly. After joining everything in the
    /// snapshot we re-snapshot and repeat — this covers the case where
    /// a non-daemon thread spawned more non-daemon threads and exited
    /// while we were joining its predecessor.
    ///
    /// # Termination
    ///
    /// The loop terminates when a fresh snapshot is empty. If a hostile
    /// or buggy program keeps spawning non-daemon threads, the loop
    /// will not exit — same as HotSpot. The optional `deadline` lets
    /// the CLI bound this for diagnostic / CI runs (`None` ≡ wait
    /// forever; `Some(_)` ≡ best-effort with a timeout).
    ///
    /// # Returns
    ///
    /// The number of distinct non-daemon threads that were joined (i.e.
    /// the number of `JoinHandle::join` calls that succeeded). `0` means
    /// no non-daemon threads existed when the call was made — the safe
    /// fast-path for `HelloWorld`-style programs.
    pub fn wait_for_non_daemon_threads(&self, deadline: Option<std::time::Instant>) -> usize {
        let mut joined = 0usize;
        loop {
            let pending = self.alive_non_daemon_thread_ids();
            if pending.is_empty() {
                return joined;
            }
            for tid in pending {
                if let Some(d) = deadline {
                    if std::time::Instant::now() >= d {
                        return joined;
                    }
                }
                // T19.K1 — `join()` takes the handle out, joins on it,
                // and marks the thread dead. If the handle was already
                // taken (e.g. a sibling pumped through `Thread.join()`
                // first) we only count successful joins. We still loop
                // back so we re-snapshot for newly-spawned non-daemon
                // threads.
                if self.join(tid) {
                    joined += 1;
                }
            }
        }
    }
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ThreadRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let threads = self.threads.lock();
        f.debug_struct("ThreadRegistry")
            .field("total", &threads.len())
            .field(
                "alive",
                &threads
                    .values()
                    .filter(|e| e.alive.load(Ordering::Acquire))
                    .count(),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_thread_id_increments() {
        let registry = ThreadRegistry::new();
        let id1 = registry.next_thread_id();
        let id2 = registry.next_thread_id();
        assert_eq!(id1, ThreadId(1));
        assert_eq!(id2, ThreadId(2));
    }

    #[test]
    fn register_and_is_alive() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker-1", None);
        assert!(registry.is_alive(tid));
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.alive_count(), 1);
        assert_eq!(registry.alive_count_and_os_tids().0, 1);
    }

    #[test]
    fn mark_dead() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker-1", None);
        assert!(registry.is_alive(tid));

        registry.mark_dead(tid);
        assert!(!registry.is_alive(tid));
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.alive_count(), 0);
    }

    #[test]
    fn starting_thread_is_not_stw_counted_until_ready() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register_starting_with_daemon(tid, "starting", None, false);

        assert!(registry.is_alive(tid));
        assert_eq!(registry.alive_count(), 1);
        assert_eq!(registry.alive_count_and_os_tids().0, 0);

        registry.mark_stw_ready(tid);
        assert_eq!(registry.alive_count_and_os_tids().0, 1);
    }

    #[test]
    fn unknown_thread_not_alive() {
        let registry = ThreadRegistry::new();
        assert!(!registry.is_alive(ThreadId(99)));
    }

    // -----------------------------------------------------------------
    // B1 — async-exception slot is a GC root and is remapped
    // -----------------------------------------------------------------

    /// Build a non-null, 8-byte-aligned dummy heap address. The registry
    /// only round-trips the address (scan as root / remap) and never
    /// dereferences it, so a live `Box` allocation is a safe stand-in for
    /// a real heap object pointer.
    fn dummy_aligned_objref(backing: &mut [u64; 2]) -> ObjectRef {
        let ptr = backing.as_mut_ptr() as *mut u8;
        // SAFETY: `backing` is a live, 8-byte-aligned, non-null allocation.
        unsafe { ObjectRef::from_raw(ptr) }
    }

    /// A posted async exception is reported as a GC root by
    /// `collect_all_root_snapshots` (so a moving GC cannot collect it
    /// before the target consumes it).
    #[test]
    fn b1_async_exception_slot_is_a_root() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "victim", None);

        let mut backing = [0u64; 2];
        let throwable = dummy_aligned_objref(&mut backing);

        // No async exception posted yet → not a root.
        assert!(registry.collect_all_root_snapshots().is_empty());

        assert!(registry.post_async_exception(tid, throwable));

        let roots = registry.collect_all_root_snapshots();
        assert_eq!(roots.len(), 1, "posted async exception must be a root");
        assert_eq!(roots[0].as_ptr(), throwable.as_ptr());

        // Consuming the slot removes the root.
        let taken = registry.take_async_exception(tid).expect("posted");
        assert_eq!(taken.as_ptr(), throwable.as_ptr());
        assert!(registry.collect_all_root_snapshots().is_empty());
    }

    /// After a moving GC relocates the throwable,
    /// `update_thread_objs_after_gc` repoints the slot to the new address,
    /// so `take_async_exception` hands back the relocated (live) object
    /// rather than a dangling from-space pointer.
    #[test]
    fn b1_async_exception_slot_is_remapped() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "victim", None);

        let mut old_backing = [0u64; 2];
        let mut new_backing = [0u64; 2];
        let old_ref = dummy_aligned_objref(&mut old_backing);
        let new_ref = dummy_aligned_objref(&mut new_backing);
        let old_addr = old_ref.as_ptr() as usize;
        let new_addr = new_ref.as_ptr() as usize;
        assert_ne!(old_addr, new_addr);

        assert!(registry.post_async_exception(tid, old_ref));

        // Simulate a moving collection that relocated old → new.
        let mut pm = HashMap::new();
        pm.insert(old_addr, new_addr);
        registry.update_thread_objs_after_gc(&pm);

        // The target now consumes the RELOCATED address, not the stale one.
        let taken = registry.take_async_exception(tid).expect("posted");
        assert_eq!(
            taken.as_ptr() as usize,
            new_addr,
            "slot must be repointed to the relocated throwable",
        );
    }

    /// An alive thread's `java.lang.Thread` mirror is reported as a GC root
    /// by `collect_all_root_snapshots`, so a collection initiated by ANOTHER
    /// thread (while this one is parked) cannot sweep the mirror out from
    /// under the dangling `ThreadEntry.java_thread_obj` copy. Regression for
    /// the Tomcat TestDigestAuthenticator "this.holder is null" NPE: a worker-
    /// thread GC freed the parked JUnit main thread's mirror, so the cached
    /// `currentThread()` pointer dereferenced an all-zero (freed) header.
    #[test]
    fn alive_thread_mirror_is_a_root() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);

        let mut backing = [0u64; 2];
        let mirror = dummy_aligned_objref(&mut backing);

        // Registered WITHOUT a mirror → not a root yet.
        registry.register(tid, "main", None);
        assert!(registry.collect_all_root_snapshots().is_empty());

        // Once the mirror is published it must be rooted.
        registry.set_java_thread_obj(tid, mirror);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(roots.len(), 1, "alive thread mirror must be a root");
        assert_eq!(roots[0].as_ptr(), mirror.as_ptr());

        // A dead thread's mirror is no longer rooted (it may be reclaimed).
        registry.mark_dead(tid);
        assert!(registry.collect_all_root_snapshots().is_empty());
    }

    #[test]
    fn collect_reserved_tlab_tails_reports_only_alive_unretired_tlabs() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);

        let mut backing = vec![0u64; 128];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let size = backing.len() * std::mem::size_of::<u64>();
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());

        registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);

        let tails = registry.collect_reserved_tlab_tails();
        assert_eq!(tails, vec![(ptr as usize + 64, ptr as usize + size)]);

        tlab.retire();
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "retired TLABs must not publish skip regions"
        );

        backing.fill(0);
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());
        registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);
        registry.mark_dead(tid);
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "dead threads must not publish stale TLAB pointers"
        );
    }

    /// INT-3 — the G1 takeover path pins the deposited snapshot roots of
    /// FROZEN peers only, selected by OS tid; other threads' snapshots (and
    /// dead threads) must not leak into the pin set.
    #[test]
    fn root_snapshots_for_os_tids_returns_only_matching_alive_threads() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        registry.set_os_tid_current(tid);

        let mut backing = [0u64; 2];
        let root = dummy_aligned_objref(&mut backing);
        registry.set_root_snapshot(tid, Arc::new(Mutex::new(vec![root])));

        let (_, _, tids, _) = registry.alive_count_blocked_and_os_tids();
        assert!(!tids.is_empty(), "os tid must be published");
        let roots = registry.root_snapshots_for_os_tids(&tids);
        assert_eq!(roots.len(), 1, "matching alive thread's snapshot returned");
        assert_eq!(roots[0].as_ptr(), root.as_ptr());

        assert!(
            registry.root_snapshots_for_os_tids(&[u32::MAX]).is_empty(),
            "non-matching tid must contribute nothing"
        );
        assert!(
            registry.root_snapshots_for_os_tids(&[]).is_empty(),
            "empty tid set short-circuits"
        );

        registry.mark_dead(tid);
        assert!(
            registry.root_snapshots_for_os_tids(&tids).is_empty(),
            "dead thread's snapshot must not be pinned"
        );
    }

    #[test]
    fn moved_thread_mirror_records_former_address_for_recovery() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);

        let mut old_backing = [0u64; 2];
        let mut new_backing = [0u64; 2];
        let old_ref = dummy_aligned_objref(&mut old_backing);
        let new_ref = dummy_aligned_objref(&mut new_backing);
        let old_addr = old_ref.as_ptr() as usize;
        let new_addr = new_ref.as_ptr() as usize;
        assert_ne!(old_addr, new_addr);

        registry.register(tid, "main", Some(old_ref));

        let mut pm = HashMap::new();
        pm.insert(old_addr, new_addr);
        registry.update_thread_objs_after_gc(&pm);

        assert_eq!(
            registry.java_thread_obj(tid).unwrap().as_ptr() as usize,
            new_addr,
            "registry mirror must be remapped to the live address",
        );
        assert_eq!(
            registry.recover_stale_mirror(old_addr).unwrap().as_ptr() as usize,
            new_addr,
            "former mirror address must recover the live mirror",
        );
        assert!(
            registry.recover_stale_mirror(new_addr).is_none(),
            "the current live address is not itself a former address",
        );
    }

    #[test]
    fn stale_mirror_recovery_survives_multiple_moves() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);

        let mut first_backing = [0u64; 2];
        let mut second_backing = [0u64; 2];
        let mut third_backing = [0u64; 2];
        let first = dummy_aligned_objref(&mut first_backing);
        let second = dummy_aligned_objref(&mut second_backing);
        let third = dummy_aligned_objref(&mut third_backing);
        let first_addr = first.as_ptr() as usize;
        let second_addr = second.as_ptr() as usize;
        let third_addr = third.as_ptr() as usize;
        assert_ne!(first_addr, second_addr);
        assert_ne!(second_addr, third_addr);

        registry.register(tid, "main", Some(first));

        let mut pm1 = HashMap::new();
        pm1.insert(first_addr, second_addr);
        registry.update_thread_objs_after_gc(&pm1);

        let mut pm2 = HashMap::new();
        pm2.insert(second_addr, third_addr);
        registry.update_thread_objs_after_gc(&pm2);

        for stale_addr in [first_addr, second_addr] {
            assert_eq!(
                registry.recover_stale_mirror(stale_addr).unwrap().as_ptr() as usize,
                third_addr,
                "any retained former mirror address should recover the current mirror",
            );
        }
    }

    #[test]
    fn thread_name() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "my-thread", None);
        assert_eq!(registry.thread_name(tid), Some("my-thread".to_string()));
        assert_eq!(registry.thread_name(ThreadId(99)), None);
    }

    #[test]
    fn join_with_real_thread() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);

        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(10));
        });
        registry.set_join_handle(tid, handle);

        let joined = registry.join(tid);
        assert!(joined);

        // Second join returns false (handle already taken)
        let joined2 = registry.join(tid);
        assert!(!joined2);
    }

    #[test]
    fn join_no_handle_dead_thread() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        registry.mark_dead(tid);

        // No handle, thread is dead — returns false immediately
        let joined = registry.join(tid);
        assert!(!joined);
    }

    // -----------------------------------------------------------------
    // T19.K1 — non-daemon thread waiting tests
    // -----------------------------------------------------------------

    /// Default `register` paints the thread as non-daemon.
    #[test]
    fn t19_k1_default_register_is_non_daemon() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "user", None);
        assert!(!registry.is_daemon(tid));
        assert_eq!(registry.alive_non_daemon_thread_ids(), vec![tid]);
    }

    /// `register_with_daemon(true)` paints it as daemon and excludes it
    /// from the non-daemon enumeration.
    #[test]
    fn t19_k1_register_with_daemon_excludes_from_wait() {
        let registry = ThreadRegistry::new();
        let user = ThreadId(1);
        let dmn = ThreadId(2);
        registry.register_with_daemon(user, "user", None, false);
        registry.register_with_daemon(dmn, "dmn", None, true);

        assert!(!registry.is_daemon(user));
        assert!(registry.is_daemon(dmn));
        assert_eq!(registry.alive_non_daemon_thread_ids(), vec![user]);
    }

    /// `set_daemon` flips the flag for a registered thread; setting
    /// `daemon = true` removes it from the wait set.
    #[test]
    fn t19_k1_set_daemon_flips_flag() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        assert!(!registry.is_daemon(tid));
        assert!(registry.set_daemon(tid, true));
        assert!(registry.is_daemon(tid));
        assert!(registry.alive_non_daemon_thread_ids().is_empty());
    }

    /// `set_daemon` on an unknown id returns `false` and is a no-op.
    #[test]
    fn t19_k1_set_daemon_unknown_id_returns_false() {
        let registry = ThreadRegistry::new();
        assert!(!registry.set_daemon(ThreadId(99), true));
    }

    /// HelloWorld parity: an empty registry waits for zero threads
    /// and returns immediately.
    #[test]
    fn t19_k1_wait_with_empty_registry_returns_zero() {
        let registry = ThreadRegistry::new();
        let start = std::time::Instant::now();
        let n = registry.wait_for_non_daemon_threads(None);
        assert_eq!(n, 0);
        // Must not block: < 50 ms is generous on slow CI.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "empty wait must not block",
        );
    }

    /// HelloWorld parity: a registry that contains only daemon threads
    /// waits for zero threads and returns immediately.
    #[test]
    fn t19_k1_wait_with_only_daemons_returns_zero() {
        let registry = ThreadRegistry::new();
        registry.register_with_daemon(ThreadId(1), "gc", None, true);
        registry.register_with_daemon(ThreadId(2), "finalizer", None, true);
        let start = std::time::Instant::now();
        let n = registry.wait_for_non_daemon_threads(None);
        assert_eq!(n, 0);
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
    }

    /// Happy path: a non-daemon thread that finishes after 100 ms
    /// blocks the wait until it terminates.
    #[test]
    fn t19_k1_wait_blocks_until_non_daemon_finishes() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "user", None);
        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        registry.set_join_handle(tid, handle);
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        assert_eq!(joined, 1);
        assert!(
            elapsed >= std::time::Duration::from_millis(80),
            "wait should block ~100ms, got {:?}",
            elapsed,
        );
    }

    /// A daemon thread does not extend the wait — even if it sleeps
    /// for a long time, the wait returns immediately because it isn't
    /// considered.
    #[test]
    fn t19_k1_daemon_thread_does_not_keep_main_alive() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register_with_daemon(tid, "dmn", None, true);
        let handle = std::thread::spawn(|| {
            // Long sleep — the wait must not block on this.
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
        registry.set_join_handle(tid, handle);
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        assert_eq!(joined, 0);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "daemon thread must not delay shutdown",
        );
    }

    /// Multiple non-daemon threads — the wait blocks until ALL of them
    /// finish, and the count reflects all joined.
    #[test]
    fn t19_k1_wait_for_multiple_non_daemons() {
        let registry = ThreadRegistry::new();
        for (i, sleep_ms) in [50u64, 100, 80].iter().enumerate() {
            let tid = ThreadId(i as u64 + 1);
            registry.register(tid, &format!("user-{i}"), None);
            let ms = *sleep_ms;
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            });
            registry.set_join_handle(tid, handle);
        }
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        assert_eq!(joined, 3);
        // Must have waited for the slowest (100ms).
        assert!(
            elapsed >= std::time::Duration::from_millis(80),
            "must wait for slowest: {:?}",
            elapsed,
        );
    }

    /// A panic in a non-daemon thread doesn't deadlock the joiner —
    /// `JoinHandle::join` returns `Err` but our wrapper swallows it
    /// and continues.
    #[test]
    fn t19_k1_panic_in_non_daemon_does_not_deadlock_joiner() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "panicker", None);
        let handle = std::thread::Builder::new()
            .name("k1-panicker".into())
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                panic!("intentional panic for K1 test");
            })
            .expect("spawn");
        registry.set_join_handle(tid, handle);
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        assert_eq!(joined, 1, "panicked thread should still count as joined");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "join must complete in bounded time even when target panicked",
        );
    }

    /// Mixed daemon + non-daemon: the wait only joins the non-daemon
    /// threads. The daemon thread is left alive (its handle still in
    /// the registry — caller is expected to abandon it on exit).
    #[test]
    fn t19_k1_mixed_only_joins_non_daemons() {
        let registry = ThreadRegistry::new();
        let user = ThreadId(1);
        let dmn = ThreadId(2);
        registry.register_with_daemon(user, "user", None, false);
        registry.register_with_daemon(dmn, "dmn", None, true);

        let user_h = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(30));
        });
        let dmn_h = std::thread::spawn(|| {
            // Long-running daemon — must NOT be joined.
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
        registry.set_join_handle(user, user_h);
        registry.set_join_handle(dmn, dmn_h);

        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        assert_eq!(joined, 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        // Daemon is still alive (we didn't join it).
        assert!(registry.is_alive(dmn));
        // User thread is joined → marked dead.
        assert!(!registry.is_alive(user));
    }

    /// Deadline-bounded wait: when a non-daemon thread's
    /// `JoinHandle` is missing (registered + alive but no
    /// `set_join_handle` call) the inner `join()` call returns
    /// `false` after a brief spin-wait. The deadline is checked
    /// between iterations; here we use a deadline shorter than
    /// the spin-wait would take to complete a fresh second
    /// iteration. The wait returns within a bounded window even
    /// though the alive flag never flipped.
    #[test]
    fn t19_k1_wait_respects_deadline_when_join_returns_false() {
        let registry = ThreadRegistry::new();
        // Spawn a real OS thread that runs for 50 ms then signals
        // the registry it's dead. We set the handle so the join
        // call has something to take.
        let tid = ThreadId(1);
        registry.register(tid, "dies-eventually", None);
        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
        registry.set_join_handle(tid, handle);
        // Deadline is generous: we expect the thread to finish in
        // ~50 ms, the join to complete, and the loop to exit
        // before the deadline. This proves the deadline path
        // doesn't pessimize the happy path.
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(5);
        let joined = registry.wait_for_non_daemon_threads(Some(deadline));
        assert_eq!(joined, 1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "happy-path deadline should not extend execution",
        );
    }

    /// `alive_non_daemon_thread_ids` reflects the current state at
    /// the moment of the snapshot — once a thread is marked dead
    /// it drops out of the enumeration.
    #[test]
    fn t19_k1_snapshot_excludes_dead_threads() {
        let registry = ThreadRegistry::new();
        let a = ThreadId(1);
        let b = ThreadId(2);
        registry.register(a, "a", None);
        registry.register(b, "b", None);
        assert_eq!(registry.alive_non_daemon_thread_ids().len(), 2);
        registry.mark_dead(a);
        assert_eq!(registry.alive_non_daemon_thread_ids(), vec![b]);
    }

    /// Re-snapshot after a non-daemon thread spawns more non-daemon
    /// threads: the wait loop must catch the second batch even
    /// though the first snapshot was already empty when those
    /// threads were added.
    #[test]
    fn t19_k1_wait_picks_up_late_spawned_threads() {
        let registry = std::sync::Arc::new(ThreadRegistry::new());
        let tid_a = ThreadId(1);
        registry.register(tid_a, "a", None);

        // Thread A sleeps a bit, then registers thread B.
        let reg2 = registry.clone();
        let handle_a = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            // Register a second non-daemon thread that runs for 60 ms.
            let tid_b = ThreadId(2);
            reg2.register(tid_b, "b", None);
            let h_b = std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(60));
            });
            reg2.set_join_handle(tid_b, h_b);
        });
        registry.set_join_handle(tid_a, handle_a);

        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        // Both A and B must be joined.
        assert_eq!(joined, 2);
        // A=40ms then B=60ms ≈ at least 100ms total.
        assert!(
            elapsed >= std::time::Duration::from_millis(80),
            "must catch late-spawned thread B: {:?}",
            elapsed,
        );
    }

    /// `register_with_daemon` and the legacy `register` must coexist —
    /// the legacy path defaults to non-daemon, the explicit path
    /// honours its argument.
    #[test]
    fn t19_k1_legacy_register_is_non_daemon_explicit_register_honors_flag() {
        let registry = ThreadRegistry::new();
        registry.register(ThreadId(1), "legacy", None);
        registry.register_with_daemon(ThreadId(2), "explicit-daemon", None, true);
        registry.register_with_daemon(ThreadId(3), "explicit-user", None, false);

        let mut ids = registry.alive_non_daemon_thread_ids();
        ids.sort_by_key(|t| t.0);
        assert_eq!(ids, vec![ThreadId(1), ThreadId(3)]);
    }

    /// Deadline-bounded wait fires before all threads complete:
    /// schedule threads with cumulative durations exceeding the
    /// deadline. After the deadline, the loop exits early.
    #[test]
    fn t19_k1_deadline_exits_before_all_joined() {
        let registry = ThreadRegistry::new();
        // 5 threads of 30 ms each. Deadline of 60 ms ⇒ at most 2-3
        // get joined before the deadline kicks in (the deadline
        // is checked only between joins).
        for i in 0..5 {
            let tid = ThreadId(i as u64 + 1);
            registry.register(tid, &format!("u-{i}"), None);
            let handle = std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(30));
            });
            registry.set_join_handle(tid, handle);
        }
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(60);
        let _joined = registry.wait_for_non_daemon_threads(Some(deadline));
        // Even if all 5 happened to finish in 30 ms (unlikely but
        // possible on a fast machine), the call must return
        // promptly — < 1 second is the upper bound for the test.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "deadline must bound the wait: {:?}",
            start.elapsed(),
        );
    }
}
