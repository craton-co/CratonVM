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
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::threading::jvm_thread::{GcBlockState, ParkState, ThreadId};
use crate::types::ObjectRef;

// ---------------------------------------------------------------------------
// CRATONVM_DBG_THREADREG_PERF — cumulative cost instrumentation for the
// full-registry-walk functions called on every GC / STW pause
// (`collect_all_root_snapshots`, `alive_count_and_os_tids`,
// `alive_count_blocked_and_os_tids`). Investigating a CratonVM-specific
// per-alive-thread VM overhead gap (Cluster B,
// fixed-suite-bugs/http-client-connector-teardown-hang-crash-FIXED.md):
// real HotSpot finishes a test class that briefly accumulates ~750 mostly-
// idle threads in 6s; CratonVM takes 25-300+s for the identical thread
// count. This measures which of these O(N) walkers actually dominates
// wall-clock, rather than guessing from a stack dump. Zero cost when unset
// (one relaxed atomic load per call to check the gate).
mod threadreg_perf {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Instant;

    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_DBG_THREADREG_PERF")
                .map(|v| {
                    let t = v.trim();
                    !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
                })
                .unwrap_or(false)
        })
    }

    pub struct Counters {
        calls: AtomicU64,
        nanos: AtomicU64,
        entries: AtomicU64,
        name: &'static str,
        started_report: AtomicBool,
    }

    impl Counters {
        pub const fn new(name: &'static str) -> Self {
            Self {
                calls: AtomicU64::new(0),
                nanos: AtomicU64::new(0),
                entries: AtomicU64::new(0),
                name,
                started_report: AtomicBool::new(false),
            }
        }

        /// Time a closure, accumulating (call count, nanos, entries scanned).
        /// `entries` is the registry size AT THIS CALL (not a running total) —
        /// used to report an average N alongside cumulative cost. Prints a
        /// running summary every 200 calls so a long test run shows progress
        /// without waiting for process exit (which this diagnostic-only VM
        /// mostly doesn't reach cleanly on a hang anyway).
        #[inline]
        pub fn time<R>(&self, entries: usize, f: impl FnOnce() -> R) -> R {
            if !enabled() {
                return f();
            }
            let start = Instant::now();
            let r = f();
            let elapsed = start.elapsed().as_nanos() as u64;
            let calls = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let nanos = self.nanos.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
            let total_entries =
                self.entries.fetch_add(entries as u64, Ordering::Relaxed) + entries as u64;
            if calls == 1 || calls % 50 == 0 {
                self.started_report.store(true, Ordering::Relaxed);
                eprintln!(
                    "[threadreg-perf] {} calls={} total_ms={:.1} avg_us={:.1} avg_registry_size={:.0} last_registry_size={}",
                    self.name,
                    calls,
                    nanos as f64 / 1_000_000.0,
                    (nanos as f64 / 1000.0) / calls as f64,
                    total_entries as f64 / calls as f64,
                    entries,
                );
            }
            r
        }
    }
}

/// The calling OS thread's id, in the same encoding `ThreadEntry::os_tid`
/// stores (`GetCurrentThreadId` on Windows, `gettid` on Linux) — see
/// `ThreadRegistry::set_os_tid_current`, whose publication this reads back.
/// `0` means "this platform has no takeover/identity backend", which every
/// caller must read as "unknown": `0` is also the never-published sentinel in
/// the entry.
#[cfg(windows)]
#[inline]
fn current_os_tid() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    unsafe { GetCurrentThreadId() }
}

/// Linux variant of [`current_os_tid`].
#[cfg(target_os = "linux")]
#[inline]
fn current_os_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

/// Fallback for platforms with no `os_tid` publication (the takeover machinery
/// is Windows/Linux-only, and so is `set_os_tid_current`).
#[cfg(all(not(windows), not(target_os = "linux")))]
#[inline]
fn current_os_tid() -> u32 {
    0
}

/// An entry in the thread registry for one JVM thread.
struct ThreadEntry {
    /// Human-readable name.
    name: String,
    /// OS thread join handle (None for the main thread).
    join_handle: Option<JoinHandle<()>>,
    /// The Java `Thread` object on the heap.
    java_thread_obj: Option<ObjectRef>,
    /// The Java-side `Thread.tid` value of `java_thread_obj` (real-JDK mode).
    /// `Thread.tid` is `final`, assigned once in the constructor from a
    /// process-wide monotonic counter, and NEVER reused across Thread
    /// objects — unlike a heap address, which the collector can recycle for
    /// a brand-new `Thread` after this entry's mirror dies. All identity
    /// lookups (getState/isAlive/interrupt/unpark) therefore prefer this
    /// key over pointer comparison; see `find_thread_id_by_java_tid` and
    /// the aliasing incident writeup in
    /// fixed-suite-bugs/tomcat/dohead-residual-http2-midrun-hang-FIXED.md.
    /// 0 = unknown (synthetic-layout mirror, or registered mid-construction
    /// before the ctor assigned `tid` — backfilled lazily on first lookup).
    java_tid: u64,
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
    /// JMX diagnostic roots. These are deliberately separate from a blocked
    /// thread's frame snapshot: a lock relationship must remain observable
    /// while the owner is running, and must be remapped across a moving GC.
    jmx_contended_monitor: Mutex<Option<ObjectRef>>,
    jmx_waiting_monitor: Mutex<Option<ObjectRef>>,
    jmx_locked_monitors: Mutex<Vec<ObjectRef>>,
    /// Shared, so the owning thread can reach its own list without going
    /// through [`ThreadRegistry::threads`] — see
    /// [`ThreadRegistry::jmx_locked_synchronizers_of`].
    jmx_locked_synchronizers: Arc<Mutex<Vec<ObjectRef>>>,
    jmx_contended_started: Mutex<Option<Instant>>,
    jmx_wait_started: Mutex<Option<Instant>>,
    jmx_blocked_count: AtomicU64,
    jmx_blocked_nanos: AtomicU64,
    jmx_waited_count: AtomicU64,
    jmx_waited_nanos: AtomicU64,
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
    /// XT-FRAME-SCAN: the owning thread's whole `JvmThread` address (0 =
    /// unpublished). Published with the same discipline as `tlab_addr`
    /// (address-stable for the thread's life, published by the owner) and
    /// cleared together with it in [`ThreadRegistry::clear_tlab_addr`]; read
    /// only for OS-frozen takeover peers (see `frozen_peer_thread_addrs`).
    jvm_thread_addr: std::sync::atomic::AtomicUsize,
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
    ///
    /// ARCH-2026-07-26 — `RwLock`, not `Mutex`. CratonVM runs one real OS
    /// thread per Java thread, and of the ~50 accessors on this type only 14
    /// mutate the map (they are all registration / teardown / one-shot
    /// publication paths). The other ~40 — `is_alive`, `get_park_state`,
    /// `is_blocked`, `java_block_state`, `frame_trace_of`, the JMX readers, and
    /// every O(N) safepoint census the collector runs — only need to *find* an
    /// entry and then operate on the atomics and per-entry locks inside it.
    /// Under a `Mutex` all of that traffic serialized on one global lock at L5,
    /// i.e. below the levels that are held across it. As a reader-writer lock
    /// the reads proceed concurrently and only genuine registration churn
    /// excludes.
    ///
    /// This is sound to do here (and was checked site by site) because no
    /// accessor calls another accessor while holding a guard: every method
    /// either scopes the guard to a block and drops it before calling out, or
    /// holds it for a self-contained walk. `parking_lot::RwLock` reads are NOT
    /// recursion-safe, so a future edit that nests two acquisitions on one
    /// thread would deadlock — keep the "acquire, use, drop, then call out"
    /// discipline, or use `read_recursive()` deliberately.
    ///
    /// Level: L5 `thread_registry` (see `cratonvm_types::lock_order`). This
    /// lock is not yet wrapped in `OrderedPlRwLock`, so the checker does not
    /// observe it — the level is documentation, exactly as it was before.
    ///
    /// AUTO-TRAIT NOTE: this raises a bound. `Mutex<T>: Sync` needs only
    /// `T: Send`, but `RwLock<T>: Sync` needs `T: Send + Sync`, so
    /// [`ThreadEntry`] must now be `Sync` for `ThreadRegistry` to be `Sync` —
    /// which `vm::vm::realms::ThreadRealm` requires, since it holds one by
    /// value inside the shared VM. It is, but only because
    /// `ObjectRef` carries an `unsafe impl Sync` (`types/src/value.rs`), which
    /// the bare `java_thread_obj: Option<ObjectRef>` field relies on, and
    /// because `std::thread::JoinHandle<T>` is unconditionally `Sync`. If
    /// either ever changes, put those two fields behind their own locks rather
    /// than reverting this one.
    threads: RwLock<FxHashMap<ThreadId, ThreadEntry>>,
    next_id: AtomicU64,
    /// Process-unique identity for this registry instance, used to key the
    /// per-thread self-handle cache (see [`ThreadRegistry::self_async_slot`]).
    ///
    /// Deliberately NOT the registry's address: a dropped registry's address
    /// can be reused by a later one, and `ThreadId`s restart per registry, so
    /// an address-keyed cache could serve one registry's slot to another's
    /// same-numbered thread. A monotonic id cannot alias.
    registry_id: u64,
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
    /// Reverse index from Java-side `Thread.tid` (unique, never reused —
    /// see `ThreadEntry::java_tid`) to registry `ThreadId`. This is the
    /// aliasing-proof identity map: a dead thread's entry stays here for
    /// TERMINATED `getState()` answers, but a NEW `Thread` allocated at the
    /// dead mirror's recycled heap address has a different `tid` and can
    /// never be confused with it (the address-keyed maps can).
    java_tid_to_id: Mutex<FxHashMap<u64, ThreadId>>,
    /// Vacated heap addresses of relocated `java.lang.Thread` mirrors →
    /// owning thread id, for stale-receiver recovery (see
    /// [`Self::recover_stale_mirror`]). Populated by
    /// `update_thread_objs_after_gc` every time a moving / promoting young
    /// collection relocates a mirror: the address the GC vacated. A running
    /// or blocked frame that resumed holding a not-yet-remapped copy of that
    /// OLD address (the frame/operand remap-coverage gap documented in
    /// `fixed-suite-bugs/gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md`)
    /// can then recover the live mirror instead of reading a zeroed object's
    /// null `holder` and NPEing in `Thread.getThreadGroup` (Tomcat
    /// `TestDigestAuthenticator` et al.). `.0` is the lookup map; `.1` is the
    /// FIFO eviction order bounding it to `FORMER_MIRROR_CAP` entries.
    former_mirror_addrs: Mutex<(
        FxHashMap<usize, ThreadId>,
        std::collections::VecDeque<usize>,
    )>,
    /// Reverse index from an owned `AbstractOwnableSynchronizer`'s heap address
    /// to the thread its `jmx_locked_synchronizers` list currently records it
    /// on — the same shape, and for the same reason, as `thread_obj_to_park`
    /// above: without it, one AQS ownership transition costs Θ(threads) map
    /// walks and mutex acquisitions, and there are two of them per uncontended
    /// `ReentrantLock.lock()`/`unlock()` pair.
    ///
    /// Strictly derived state. [`Self::set_jmx_owned_synchronizer`] is its only
    /// producer, and it updates this map and the per-thread lists together;
    /// [`Self::update_thread_objs_after_gc`] rekeys it whenever a moving
    /// collection relocates a recorded synchronizer, in the same pass that
    /// remaps the lists themselves. An entry naming a reaped thread is inert —
    /// `threads.get` answers `None` and the removal is skipped, exactly as the
    /// linear scan used to skip a missing entry.
    synchronizer_owner: Mutex<FxHashMap<usize, ThreadId>>,
}

/// Upper bound on retained former-mirror addresses (see
/// [`ThreadRegistry::former_mirror_addrs`]). Mirrors relocate rarely, and a
/// stale frame copy is only consultable until its slot is reclaimed/reused,
/// so a few thousand recent vacated addresses is ample; the FIFO bound keeps
/// the table from growing across a long-running process.
const FORMER_MIRROR_CAP: usize = 8192;

// A terminated Java thread's observable state lives in its mirror. Keeping a
// registry entry after its carrier has exited is therefore unnecessary: all
// registry-backed operations on a missing entry already have the terminated
// semantics (not alive, no interrupt target, join returns immediately). More
// importantly, every GC/STW census walks this map. Tomcat's non-blocking test
// repeatedly creates and tears down endpoint pools, and retaining every dead
// carrier turned those censuses into an ever-growing O(total threads ever
// created) cost. Keep a small diagnostic tail while bounding that cost.
const TERMINATED_THREAD_ENTRY_CAP: usize = 256;

impl ThreadRegistry {
    /// Create a new, empty registry. The next thread id will be 1
    /// (id 0 is reserved for the main thread).
    pub fn new() -> Self {
        static NEXT_REGISTRY_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            threads: RwLock::new(FxHashMap::default()),
            next_id: AtomicU64::new(1),
            registry_id: NEXT_REGISTRY_ID.fetch_add(1, Ordering::Relaxed),
            thread_obj_to_park: Mutex::new(FxHashMap::default()),
            java_tid_to_id: Mutex::new(FxHashMap::default()),
            former_mirror_addrs: Mutex::new((
                FxHashMap::default(),
                std::collections::VecDeque::new(),
            )),
            synchronizer_owner: Mutex::new(FxHashMap::default()),
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
            java_tid: 0,
            alive: AtomicBool::new(true),
            stw_ready: AtomicBool::new(stw_ready),
            daemon: AtomicBool::new(daemon),
            park_state: park_state.clone(),
            interrupted: Arc::new(AtomicBool::new(false)),
            root_snapshot: Arc::new(Mutex::new(Vec::new())),
            frame_trace: Arc::new(Mutex::new(Vec::new())),
            vm_state: Arc::new(Mutex::new(String::new())),
            gc_block_state: Arc::new(GcBlockState::new()),
            jmx_contended_monitor: Mutex::new(None),
            jmx_waiting_monitor: Mutex::new(None),
            jmx_locked_monitors: Mutex::new(Vec::new()),
            jmx_locked_synchronizers: Arc::new(Mutex::new(Vec::new())),
            jmx_contended_started: Mutex::new(None),
            jmx_wait_started: Mutex::new(None),
            jmx_blocked_count: AtomicU64::new(0),
            jmx_blocked_nanos: AtomicU64::new(0),
            jmx_waited_count: AtomicU64::new(0),
            jmx_waited_nanos: AtomicU64::new(0),
            async_exception_slot: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tlab_addr: std::sync::atomic::AtomicUsize::new(0),
            jvm_thread_addr: std::sync::atomic::AtomicUsize::new(0),
            os_tid: std::sync::atomic::AtomicU32::new(0),
        };
        self.threads.write().insert(thread_id, entry);
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
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(tlab_addr, Ordering::Release);
        }
    }

    /// BUG-03 — clear this thread's published TLAB address. MUST be called by
    /// the thread (or its teardown) before its `JvmThread` is dropped so the
    /// collector never dereferences a dangling TLAB pointer.
    pub fn clear_tlab_addr(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(0, Ordering::Release);
            // XT-FRAME-SCAN: the JvmThread address shares the TLAB address's
            // exact lifecycle (published once the JvmThread's address is
            // final, must stop being visible before it drops) — clear both
            // under the same lock hold so no reader can observe a cleared
            // TLAB with a still-published thread address or vice versa.
            entry.jvm_thread_addr.store(0, Ordering::Release);
        }
    }

    /// XT-FRAME-SCAN: publish the owning thread's `JvmThread` address so the
    /// cross-thread STW takeover can walk a frozen peer's interpreter frames
    /// (locals + operand stacks) directly. Same publication discipline as
    /// [`Self::set_tlab_addr`]: called once after the `JvmThread`'s address
    /// is final (worker spawn body / primordial boot / JNI attach); cleared
    /// by [`Self::clear_tlab_addr`] before the `JvmThread` drops. No-op for
    /// an unknown id.
    pub fn set_jvm_thread_addr(&self, thread_id: ThreadId, addr: usize) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.jvm_thread_addr.store(addr, Ordering::Release);
        }
    }

    /// XT-FRAME-SCAN: the published `JvmThread` addresses of every alive
    /// thread whose OS tid is in `os_tids` (the takeover's frozen-peer set).
    ///
    /// SAFETY contract for the caller's deref: entries must only be
    /// dereferenced for peers that are OS-frozen by the takeover (suspended
    /// with `Rip` inside registered compiled code) and stay frozen until
    /// after the deref ends — compiled code never mutates the `JvmThread`'s
    /// interpreter state, and a frozen thread cannot exit (so `alive` cannot
    /// flip nor the `JvmThread` drop while frozen).
    pub fn frozen_peer_thread_addrs(&self, os_tids: &[u32]) -> Vec<usize> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        let threads = self.threads.read();
        let mut out = Vec::new();
        for entry in threads.values() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            let tid = entry.os_tid.load(Ordering::Acquire);
            if tid == 0 || !os_tids.contains(&tid) {
                continue;
            }
            let addr = entry.jvm_thread_addr.load(Ordering::Acquire);
            if addr != 0 {
                out.push(addr);
            }
        }
        out
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
        let threads = self.threads.read();
        let mut out = Vec::new();
        // Census counters for the `CRATONVM_G1_DBG_REACH` line below. An empty
        // result has three very different causes and the G1 walk-break reports
        // ("ZERO published skip spans this pause") cannot tell them apart:
        // nobody is registered, everybody is registered but retired, or this
        // function was never reached at all. Counting the population separates
        // the first two, and the absence of the line entirely settles the third.
        let (mut total, mut dead, mut unregistered, mut retired) = (0usize, 0, 0, 0);
        for entry in threads.values() {
            total += 1;
            if !entry.alive.load(Ordering::Acquire) {
                dead += 1;
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                unregistered += 1;
                continue;
            }
            // SAFETY: see method contract — the owning thread is parked /
            // blocked / OS-suspended, so the `Tlab` at `addr` is live and not
            // being mutated.
            let tlab = unsafe { &*(addr as *const cratonvm_gc::Tlab) };
            match tlab.reserved_tail() {
                Some(tail) => out.push(tail),
                None => retired += 1,
            }
        }
        if cratonvm_types::flags().gc.g1_dbg_reach {
            // Sequence number, not a pause id: the walk-break reports carry no
            // pause identity either, so the only thing that can be compared is
            // "how many times did the publish path run" against "how many
            // breaks reported ZERO published skip spans". The first run
            // produced exactly ONE census line across 453 s and four
            // zero-span breaks, which is the fact this counter is here to
            // confirm or kill.
            static CENSUS_SEQ: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let seq = CENSUS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "[g1][TLAB-CENSUS] #{seq} entries={total} dead={dead} \
                 alive_no_tlab_addr={unregistered} alive_retired={retired} published={}",
                out.len()
            );
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
        let threads = self.threads.read();
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
        let threads = self.threads.read();
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
            .read()
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
        let threads = self.threads.read();
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
        let threads = self.threads.read();
        for entry in threads.values() {
            entry.park_state.unpark();
        }
    }

    /// Legend printed under the T19.H1 thread-summary header, explaining what
    /// the `deposit=` column means. See [`Self::deposit_freshness`] for why
    /// this exists at all.
    pub(crate) const DEPOSIT_LEGEND: &'static str = concat!(
        "  legend: deposit=live        — parked in a blocking native; blocked=/roots=/top= ARE its current state.\n",
        "          deposit=STALE       — RUNNING right now. top=/roots= are whatever it deposited at its\n",
        "                                LAST blocking call and can be arbitrarily old — read the live\n",
        "                                \"T19.H1 stack dump\" above for where this thread actually is,\n",
        "                                UNLESS the row says no-live-dump (see below).\n",
        "          no-live-dump        — RUNNING, and it produced no live dump: it never reached an\n",
        "                                interpreter dispatch point during the watchdog's grace period.\n",
        "                                That means JIT-COMPILED code or a long native call — NOT a\n",
        "                                deadlock, and NOT evidence about which. Re-run with --nojit:\n",
        "                                if the live dump then appears, it was compiled code.\n",
        "          deposit=post-mortem — dead. alive=false is authoritative; blocked=/roots=/top= are residue from\n",
        "                                its termination sequence (nothing clears them), NOT evidence of a wait."
    );

    /// How current a registry entry's *deposited* state — the `blocked=`,
    /// `roots=` and frame-chain columns of the T19.H1 summary — actually is.
    ///
    /// A thread deposits its root snapshot and frame chain when it is about to
    /// block, and **nothing clears either afterwards**: not on wake, not on
    /// death. So the deposit is a faithful readout of the thread's position in
    /// exactly one case — it is alive AND still inside a blocked region.
    /// Otherwise it is history:
    ///
    /// * `STALE` (alive, not blocked): the thread is executing bytecode. The
    ///   chain names its last *blocking* call, which may be seconds or minutes
    ///   old and in a completely different part of the program.
    /// * `post-mortem` (dead): `blocked=true` here is the terminal
    ///   `deposit_root_snapshot()` in the thread-exit sequence, which raises
    ///   `in_blocked_region` and has no counterpart to lower it. Every STW
    ///   census filters on `alive` before reading that flag
    ///   (`alive_count_blocked_and_os_tids_inner`, `blocked_os_tids`,
    ///   `fold_pointer_map_into_blocked`), so the residue is inert to the
    ///   barrier — it is purely a reporting artifact.
    ///
    /// Labelling this is not cosmetic. Three separate investigations have now
    /// been sent the wrong way by an unmarked stale deposit:
    /// `onclasscondition-join-never-returns-20260801` (read `alive=false` on
    /// dead entries as a missed wakeup), the Elasticsearch
    /// `ES-HANG-20260719-threadjoin` capture (same shape), and
    /// `bug-h2-teststringcache-thread-join-blocked-on-dead-threads-20260807`,
    /// where `main` was `alive=true blocked=false` — running
    /// `TestStringCache.runBenchmark()` — while its unmarked deposit still read
    /// `java/lang/Thread.join@129` from a join that had returned ~40 s earlier.
    /// The live stack dump printed directly above said `runBenchmark`; nothing
    /// flagged the contradiction, and the whole doc was written against the
    /// stale line.
    pub(crate) fn deposit_freshness(alive: bool, blocked: bool) -> &'static str {
        match (alive, blocked) {
            (false, _) => "post-mortem",
            (true, true) => "live",
            (true, false) => "STALE",
        }
    }

    /// T19.H1 watchdog — write [`Self::render_thread_summary`] to stderr.
    ///
    /// `live_dumped` is the set of thread ids that answered the watchdog with a
    /// live frame dump. See [`Self::render_thread_summary_for`].
    pub fn dump_thread_summary_to_stderr(&self, live_dumped: &[u64]) {
        use std::io::Write;
        let text = self.render_thread_summary_for(live_dumped);
        let stderr = std::io::stderr();
        let mut h = stderr.lock();
        let _ = h.write_all(text.as_bytes());
        let _ = h.flush();
    }

    /// [`Self::render_thread_summary_for`] with no live-dump information —
    /// every RUNNING thread is then reported as `no-live-dump`, which is the
    /// safe reading when the caller cannot say. Kept for tests and for callers
    /// outside the watchdog path.
    pub fn render_thread_summary(&self) -> String {
        self.render_thread_summary_for(&[])
    }

    /// T19.H1 watchdog — a one-line summary of every registered thread (name,
    /// liveness, daemon, deposited-root count), plus the complete deposited
    /// frame chain of every alive thread. Lets the watchdog show threads that
    /// have NO dumpable interpreter frames — e.g. a thread blocked in a
    /// Rust-level native lock, or one that never started — which the frame-dump
    /// path cannot surface. A non-zero `roots` on an otherwise-silent thread
    /// means it deposited roots before blocking (it IS blocked in a native),
    /// distinguishing "blocked in native" from "never ran".
    ///
    /// Every row carries a `deposit=` freshness tag; see
    /// [`Self::deposit_freshness`] for what each value licenses you to
    /// conclude, and why an untagged chain has repeatedly been misread as a
    /// live wait site.
    ///
    /// `live_dumped` lists the thread ids that produced a live frame dump in
    /// response to this watchdog cycle. It is what separates the two very
    /// different states that both render as `blocked=false`:
    ///
    ///  * a RUNNING thread that DID dump — the live dump above is where it is;
    ///  * a RUNNING thread that did NOT — it never reached an interpreter
    ///    dispatch point, i.e. it is in JIT-compiled code or a long native
    ///    call. The summary used to point every such row at a
    ///    `"T19.H1 stack dump: tid=N"` section that was never emitted, and the
    ///    absence of that section then read as "blocked somewhere the watchdog
    ///    cannot reach". It cost
    ///    `known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter`
    ///    a whole investigation: the thread was neither blocked nor in native
    ///    code, it was spinning in a compiled `ByteBuf.writeByte` loop, and
    ///    `--nojit` produced the full 73-frame dump immediately.
    ///
    /// Returned as a `String` (rather than written straight out) so the format
    /// is unit-testable — the tags below are the load-bearing part.
    pub fn render_thread_summary_for(&self, live_dumped: &[u64]) -> String {
        use std::fmt::Write as _;
        let threads = self.threads.read();
        let mut h = String::new();
        let _ = writeln!(
            h,
            "--- T19.H1 thread summary: {} registered thread(s) ---",
            threads.len()
        );
        let _ = writeln!(h, "{}", Self::DEPOSIT_LEGEND);
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
            full_frames: Vec<String>,
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
                // 2026-08-03 (onclasscondition-join-never-returns) — the
                // one-line `top` above caps at 3 frames, which is enough to
                // name a wait site but not to tell "this thread is a live
                // participant mid-call-chain" from "this is a stale dead
                // entry that happens to share a wait site with hundreds of
                // others from earlier per-test JVM reboots". Every alive
                // thread's COMPLETE deposited chain, oldest frame first (same
                // order `dump_current_thread_frames` uses), so a hang repro
                // can be told apart from the accumulated dead-thread noise
                // without a second run.
                let full_frames = if e.alive.load(std::sync::atomic::Ordering::Acquire) {
                    trace
                        .iter()
                        .map(|frame| {
                            format!(
                                "{}.{}@{}",
                                frame.class_name, frame.method_name, frame.byte_code_index
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
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
                    full_frames,
                }
            })
            .collect();
        rows.sort_by_key(|r| r.tid);
        for row in &rows {
            // `deposit=` sits immediately before `top=` on purpose: the two are
            // read together, and the tag is what stops the chain from being
            // taken for a live wait site.
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} alive={} daemon={} blocked={} roots={} state={:?} deposit={} top={}",
                row.tid,
                row.os_tid,
                row.name,
                row.alive,
                row.daemon,
                row.blocked,
                row.roots,
                row.state,
                Self::deposit_freshness(row.alive, row.blocked),
                row.top
            );
            if row.alive && !row.blocked && !live_dumped.contains(&row.tid) {
                let _ = writeln!(
                    h,
                    "        ^ no-live-dump: this thread produced NO \"T19.H1 stack dump: tid={}\" \
                     section — it never reached an interpreter dispatch point, so it is in \
                     JIT-compiled code or a long native call. Re-run with --nojit; if the live \
                     dump appears there, it was compiled code (and it was RUNNING, not stuck).",
                    row.tid
                );
            }
        }
        // Full frame chains, split out from the one-line-per-thread summary
        // above so that table stays scannable. Restricted to `alive` threads
        // — the registry retains every thread it has ever seen for the life
        // of the process, so a long-running multi-class suite run can carry
        // hundreds of dead entries whose frame chains are pure noise here.
        let live_with_frames: Vec<&SummaryRow> = rows
            .iter()
            .filter(|r| r.alive && !r.full_frames.is_empty())
            .collect();
        // "alive" is NOT the same as "parked here". A running thread keeps the
        // chain it deposited at its last blocking call, so split the count —
        // an all-STALE section is a section with no wait sites in it at all.
        let at_wait_site = live_with_frames.iter().filter(|r| r.blocked).count();
        let _ = writeln!(
            h,
            "--- T19.H1 full frame chains: {} alive thread(s) with a deposited snapshot ({} at a live wait site, {} STALE) ---",
            live_with_frames.len(),
            at_wait_site,
            live_with_frames.len() - at_wait_site
        );
        for row in live_with_frames {
            let tag = if row.blocked {
                "[deposit=live — this IS the thread's current wait site]".to_string()
            } else if live_dumped.contains(&row.tid) {
                format!(
                    "[deposit=STALE — thread is RUNNING (blocked=false); chain below is from its LAST \
                     blocking call and may be arbitrarily old. Its real position is in the \
                     \"T19.H1 stack dump: tid={}\" section above]",
                    row.tid
                )
            } else {
                // Do NOT send the reader to a section that was never emitted —
                // its absence is the finding, not a missing feature.
                "[deposit=STALE + no-live-dump — thread is RUNNING (blocked=false) and answered no \
                 dump, so it is in JIT-compiled code or a long native call. The chain below is from \
                 its LAST blocking call and may be arbitrarily old; there is NO live section for \
                 this thread. Re-run with --nojit to get one.]"
                    .to_string()
            };
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} ({} frame(s), oldest first) {}:",
                row.tid,
                row.os_tid,
                row.name,
                row.full_frames.len(),
                tag
            );
            for (depth, frame) in row.full_frames.iter().enumerate() {
                let _ = writeln!(h, "    [{depth}] {frame}");
            }
        }
        let _ = writeln!(h, "--- T19.H1 end thread summary ---");
        h
    }

    /// ARCH-2026-07-26 — the calling thread's own async-exception slot,
    /// obtained without touching the global registry map after the first call.
    ///
    /// [`Self::take_async_exception`] runs on **every safepoint poll** of
    /// **every** Java thread. Reaching it through the map meant a global lock
    /// acquisition per poll per thread — the single hottest "look up my own
    /// entry" path in the registry, and the one the per-thread-handle idea
    /// exists for.
    ///
    /// Caching is sound because `ThreadEntry::async_exception_slot` is an
    /// `Arc<AtomicUsize>` created once in `register_with_daemon_stw_ready` and
    /// **never replaced** — there is no setter for it, and `post_async_exception`
    /// writes through the very same allocation. So the cached `Arc` is the same
    /// object the map holds, forever.
    ///
    /// The cache is keyed by `(registry_id, thread_id)`. `registry_id` is a
    /// process-monotonic counter rather than the registry's address, because a
    /// dropped registry's address can be reused while `ThreadId`s restart at 1
    /// — an address key could hand one registry's slot to another registry's
    /// same-numbered thread (a real hazard in the test suite, which builds many
    /// registries on one OS thread).
    ///
    /// Reaping a terminated entry does not invalidate the cache: the `Arc`
    /// keeps the slot alive, and a thread whose entry is gone is dead, so
    /// nobody can post to it any more.
    fn self_async_slot(&self, thread_id: ThreadId) -> Option<Arc<std::sync::atomic::AtomicUsize>> {
        thread_local! {
            static CACHED: std::cell::RefCell<
                Option<(u64, ThreadId, Arc<std::sync::atomic::AtomicUsize>)>,
            > = const { std::cell::RefCell::new(None) };
        }
        CACHED.with(|c| {
            if let Some((rid, tid, slot)) = c.borrow().as_ref() {
                if *rid == self.registry_id && *tid == thread_id {
                    return Some(Arc::clone(slot));
                }
            }
            // Cold: one read-lock to fetch the `Arc`, then never again for
            // this (registry, thread) pair.
            let slot = self
                .threads
                .read()
                .get(&thread_id)
                .map(|e| Arc::clone(&e.async_exception_slot))?;
            *c.borrow_mut() = Some((self.registry_id, thread_id, Arc::clone(&slot)));
            Some(slot)
        })
    }

    /// T1.5.1 — Take the target thread's pending async exception (if any).
    /// Called from the target thread's `safepoint_check` — never
    /// cross-thread. The consumer is responsible for raising the
    /// exception via the interpreter's normal exception table walk.
    ///
    /// Because it is always a self-lookup, it goes through
    /// [`Self::self_async_slot`] and so takes **no registry lock** after the
    /// calling thread's first safepoint.
    pub fn take_async_exception(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let slot = self.self_async_slot(thread_id)?;
        let raw = slot.swap(0, std::sync::atomic::Ordering::AcqRel);
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
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.join_handle = Some(handle);
        }
    }

    /// Mark a thread as dead (called when the thread finishes execution).
    pub fn mark_dead(&self, thread_id: ThreadId) {
        let dead_mirror;
        let dead_park_state;
        let mut retired_java_tids = Vec::new();
        {
            let mut threads = self.threads.write();
            let Some(entry) = threads.get_mut(&thread_id) else {
                return;
            };
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
            dead_mirror = entry.java_thread_obj;
            dead_park_state = entry.park_state.clone();

            let dead_count = threads
                .values()
                .filter(|entry| !entry.alive.load(Ordering::Acquire))
                .count();
            if dead_count > TERMINATED_THREAD_ENTRY_CAP {
                let mut purge: Vec<ThreadId> = threads
                    .iter()
                    .filter_map(|(id, entry)| {
                        (!entry.alive.load(Ordering::Acquire)
                            && entry.async_exception_slot.load(Ordering::Acquire) == 0)
                            .then_some(*id)
                    })
                    .collect();
                purge.sort_by_key(|id| id.0);
                for id in purge
                    .into_iter()
                    .take(dead_count - TERMINATED_THREAD_ENTRY_CAP)
                {
                    if let Some(retired) = threads.remove(&id) {
                        if retired.java_tid != 0 {
                            retired_java_tids.push(retired.java_tid);
                        }
                    }
                }
            }
        }
        // Drop the dead thread's mirror-ADDRESS→ParkState reverse-index entry.
        // The entry itself is retained (TERMINATED `getState()` answers), but
        // once the mirror is unreachable the collector can recycle its heap
        // address for a brand-new `Thread`; a lingering address key would then
        // route that NEW thread's `unpark`/`interrupt` wakeups to this dead
        // ParkState — a silently lost wakeup (observed as Tomcat executor
        // workers parked forever across DoHead's 288 Tomcat start/stop cycles).
        // Guarded by Arc identity so a same-address re-registration that
        // already overwrote the slot (new thread registered before this ran)
        // is left untouched. Lock order threads → thread_obj_to_park matches
        // the register/set paths.
        if let Some(obj) = dead_mirror {
            let mut idx = self.thread_obj_to_park.lock();
            if let Some(mapped) = idx.get(&(obj.as_ptr() as usize)) {
                if Arc::ptr_eq(mapped, &dead_park_state) {
                    idx.remove(&(obj.as_ptr() as usize));
                }
            }
        }
        if !retired_java_tids.is_empty() {
            let mut by_java_tid = self.java_tid_to_id.lock();
            for java_tid in retired_java_tids {
                by_java_tid.remove(&java_tid);
            }
        }
        // P1 shadow record — the `-> Terminated` edge. `mark_dead` is NOT
        // always self-called: `ThreadRegistry::join` (below) marks the joinee
        // dead from the *joining* thread. `record_transition_for` therefore
        // records only when the calling thread's cell is bound to this id, so
        // a peer's termination can never be attributed to the caller.
        crate::threading::thread_state::record_transition_for(
            thread_id.0,
            crate::threading::thread_state::ThreadExecState::Terminated,
            "thread_registry::mark_dead",
        );
    }

    /// Aliasing-proof identity lookup: registry `ThreadId` by the Java-side
    /// `Thread.tid` value (see `ThreadEntry::java_tid`). Dead threads remain
    /// resolvable (TERMINATED), but a recycled mirror address can never alias
    /// because `tid` values are unique for the life of the process.
    pub fn find_thread_id_by_java_tid(&self, java_tid: u64) -> Option<ThreadId> {
        if java_tid == 0 {
            return None;
        }
        self.java_tid_to_id.lock().get(&java_tid).copied()
    }

    /// ParkState lookup by Java-side `Thread.tid` — the aliasing-proof
    /// counterpart of `find_park_state_by_thread_obj` (see
    /// `find_thread_id_by_java_tid`).
    pub fn find_park_state_by_java_tid(&self, java_tid: u64) -> Option<Arc<ParkState>> {
        let id = self.find_thread_id_by_java_tid(java_tid)?;
        self.get_park_state(id)
    }

    /// Pointer-identity walk like `find_thread_id_by_thread_obj`, but guarded
    /// by the caller-supplied Java `tid` of the queried mirror: an entry at
    /// the same address whose recorded `java_tid` differs is a DEAD thread's
    /// stale entry whose mirror address was recycled for the queried (new)
    /// Thread — matching it would misreport a freshly constructed thread as
    /// RUNNABLE/TERMINATED (the DoHead `IllegalThreadStateException`-at-
    /// engine-start flake). An entry with `java_tid == 0` (registered
    /// mid-construction, before the ctor assigned `tid`) is accepted and
    /// lazily backfilled so subsequent lookups take the O(1) tid index.
    pub fn find_thread_id_by_thread_obj_tid_checked(
        &self,
        obj: ObjectRef,
        java_tid: u64,
    ) -> Option<ThreadId> {
        let mut threads = self.threads.write();
        let mut found: Option<ThreadId> = None;
        for (id, entry) in threads.iter() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr())
                    && (entry.java_tid == java_tid || entry.java_tid == 0)
                {
                    found = Some(*id);
                    break;
                }
            }
        }
        if let Some(id) = found {
            if java_tid != 0 {
                let mut backfilled = false;
                if let Some(entry) = threads.get_mut(&id) {
                    if entry.java_tid == 0 {
                        entry.java_tid = java_tid;
                        backfilled = true;
                    }
                }
                drop(threads);
                if backfilled {
                    self.java_tid_to_id.lock().insert(java_tid, id);
                }
            }
        }
        found
    }

    /// Check if a thread is still alive.
    pub fn is_alive(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.alive.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Mark a started carrier as able to participate in counted STW barriers.
    pub fn mark_stw_ready(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.stw_ready.store(true, Ordering::Release);
        }
        drop(threads);
        // P1 shadow record — the `Starting -> JavaRunning` edge. Always
        // self-called by the freshly spawned carrier (`vm/src/vm/vm_exec.rs`
        // `thread_start` and the virtual-thread first-mount arm), inside
        // `GcBarrier::run_if_no_stw_requested`, so binding here is safe.
        crate::threading::thread_state::bind_current_thread(thread_id.0);
        crate::threading::thread_state::record_transition(
            crate::threading::thread_state::ThreadExecState::JavaRunning,
            "thread_registry::mark_stw_ready",
        );
    }

    /// Block the calling OS thread until the target thread finishes.
    ///
    /// Takes the `JoinHandle` out of the registry and joins on it.
    /// Returns `true` if the join succeeded, `false` if no handle was found
    /// (already joined, or it's the main thread).
    pub fn join(&self, thread_id: ThreadId) -> bool {
        let handle = {
            let mut threads = self.threads.write();
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
            .read()
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

    /// Set the Java Thread object for a given ThreadId, recording the
    /// mirror's Java-side `Thread.tid` (0 = unknown; see
    /// `ThreadEntry::java_tid`) so identity lookups can use the
    /// aliasing-proof tid index instead of the mirror's (recyclable) address.
    pub fn set_java_thread_obj_with_tid(&self, thread_id: ThreadId, obj: ObjectRef, java_tid: u64) {
        self.set_java_thread_obj(thread_id, obj);
        if java_tid != 0 {
            {
                let mut threads = self.threads.write();
                if let Some(entry) = threads.get_mut(&thread_id) {
                    entry.java_tid = java_tid;
                } else {
                    return;
                }
            }
            self.java_tid_to_id.lock().insert(java_tid, thread_id);
        }
    }

    /// Set the Java Thread object for a given ThreadId.
    pub fn set_java_thread_obj(&self, thread_id: ThreadId, obj: ObjectRef) {
        let park_state_clone;
        let prev_obj;
        {
            let mut threads = self.threads.write();
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
        self.threads.read().get(&thread_id).map(|e| e.name.clone())
    }

    /// The Java-side `Thread.tid` for a registry id, or `None` when this entry
    /// has no Java mirror yet (see [`ThreadEntry::java_tid`], 0 = unknown).
    ///
    /// Needed because `ThreadJmxSnapshot::thread_id` carries the JAVA tid
    /// while monitor ownership is keyed by the registry `ThreadId`. Putting a
    /// registry id in `lock_owner_id` would mix two numbering schemes in one
    /// field, and `findDeadlockedThreads` matches `lock_owner_id` against
    /// other threads' `thread_id` to build its wait-for graph — so the
    /// mismatch would silently yield no edges rather than a visible error.
    pub fn java_tid_of(&self, thread_id: ThreadId) -> Option<u64> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.java_tid)
            .filter(|tid| *tid != 0)
    }

    /// The OS-level thread id for a registry id, or `None` if this entry has
    /// not published one yet (0 = unset).
    ///
    /// This is what an ARBITRARY-thread CPU-time read needs — `GetThreadTimes`
    /// after `OpenThread` on Windows, `/proc/self/task/<tid>/stat` on Linux.
    /// `ThreadMXBean.isThreadCpuTimeSupported()` answered false purely because
    /// this was not reachable from a native, not because the VM lacked the
    /// datum: `set_os_tid_current` has been publishing it all along.
    pub fn os_tid_of(&self, thread_id: ThreadId) -> Option<u32> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.os_tid.load(Ordering::Acquire))
            .filter(|tid| *tid != 0)
    }

    /// Publish a monitor acquisition attempt before it can block. The object
    /// is rooted by this registry entry until the acquire completes.
    pub fn set_jmx_contended_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_contended_monitor.lock() = Some(monitor);
            *entry.jmx_contended_started.lock() = Some(Instant::now());
            // Count the block HERE, not on release. The JMM counts a thread as
            // having blocked the moment it starts waiting, and a JMX consumer
            // diagnosing a hang reads `getBlockedCount()` while the thread is
            // STILL blocked — counting on the way out leaves exactly that case
            // reporting zero, which is the case that matters. Measured: a
            // thread parked on a held monitor reported `blockedCount == 0` on
            // every thread in the VM until this moved.
            entry.jmx_blocked_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Finish an acquisition attempt. Successful acquisitions become owned
    /// monitor roots; failed/aborted attempts simply lose the contention root.
    pub fn complete_jmx_monitor_enter(&self, thread_id: ThreadId, monitor: ObjectRef) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_contended_monitor.lock() = None;
            // Only the DURATION is added here — the count was taken when the
            // block began. `jmx_contended_started` is `None` on the uncontended
            // arm (which calls this to publish ownership), so nothing accrues
            // for an acquisition that never waited.
            if let Some(start) = entry.jmx_contended_started.lock().take() {
                entry.jmx_blocked_nanos.fetch_add(
                    start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
            }
            let mut owned = entry.jmx_locked_monitors.lock();
            if !owned.iter().any(|o| o.as_ptr() == monitor.as_ptr()) {
                owned.push(monitor);
            }
        }
    }

    pub fn remove_jmx_locked_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            entry
                .jmx_locked_monitors
                .lock()
                .retain(|o| o.as_ptr() != monitor.as_ptr());
        }
    }

    pub fn set_jmx_waiting_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_waiting_monitor.lock() = Some(monitor);
            *entry.jmx_wait_started.lock() = Some(Instant::now());
        }
    }

    /// The monitor `thread_id` is currently blocked in `Object.wait()` on, if
    /// any — without consuming it or closing the JMX wait-time accounting.
    ///
    /// Used by `Thread.interrupt()` to wake a target parked in `Object.wait()`.
    /// The slot is written just before the park and taken just after it, so a
    /// `Some` here means the target is (or was a moment ago) in the wait, and a
    /// wake sent to a target that has already left is harmless: `wait()` is
    /// specified to permit spurious wakeups, and the surrounding Java `while`
    /// loop re-checks and re-parks.
    pub fn peek_jmx_waiting_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.read();
        let monitor = *threads.get(&thread_id)?.jmx_waiting_monitor.lock();
        monitor
    }

    /// EVERY registered thread that is currently parked in `Object.wait()`,
    /// with the object each one is parked on.
    ///
    /// # Why a census, and not the one thread the watchdog reaches
    ///
    /// The wait-site dump reports the FIRST parked thread it finds, which on
    /// the netty `ParameterizedSslHandlerTest` stalls was always the test thread
    /// — and the test thread is parked on a promise that something *else* was
    /// supposed to complete. A dump that names only that thread cannot tell
    /// "nothing ever completed this promise" from "the thread that would have
    /// completed it is itself parked somewhere", and those two need opposite
    /// investigations. One line per waiter settles it inside the stall that is
    /// already happening, with no second run.
    ///
    /// Rows are `(tid, name, alive, waited_on, waited_ms)`, sorted by tid.
    /// `waited_ms` is `0` when the wait-start stamp is missing, so a caller
    /// never has to unwrap it. The `ObjectRef` comes from the same
    /// `jmx_waiting_monitor` slot [`Self::peek_jmx_waiting_monitor`] reads —
    /// a scanned root that `update_thread_objs_after_gc` forwards — so it is
    /// sound to dereference under a moving collector, unlike `Monitor::wait`'s
    /// own entry-time local.
    pub fn waiting_monitor_census(&self) -> Vec<(u64, String, bool, ObjectRef, u64)> {
        let threads = self.threads.read();
        let mut out: Vec<(u64, String, bool, ObjectRef, u64)> = Vec::new();
        for (tid, entry) in threads.iter() {
            let waiting = *entry.jmx_waiting_monitor.lock();
            let Some(obj) = waiting else {
                continue;
            };
            let started = *entry.jmx_wait_started.lock();
            let waited_ms = started
                .map(|t| t.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0);
            out.push((
                tid.0,
                entry.name.clone(),
                entry.alive.load(Ordering::Acquire),
                obj,
                waited_ms,
            ));
        }
        out.sort_by_key(|r| r.0);
        out
    }

    pub fn take_jmx_waiting_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        let monitor = entry.jmx_waiting_monitor.lock().take();
        if let Some(start) = entry.jmx_wait_started.lock().take() {
            entry.jmx_waited_count.fetch_add(1, Ordering::Relaxed);
            entry.jmx_waited_nanos.fetch_add(
                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
        monitor
    }

    /// `AbstractOwnableSynchronizer` has one exclusive owner. Remove a
    /// synchronizer from any former owner before attaching it to the new one.
    ///
    /// # Why this keeps a reverse index instead of scanning
    ///
    /// This runs on **every AQS ownership transition** — `acquire` passes the
    /// owner, `release` passes `null`, so it is twice per uncontended
    /// `ReentrantLock.lock()`/`unlock()` pair (censused in
    /// `probes/LockNativeCensusProbe.java`). The original body walked EVERY
    /// registered thread and took EVERY thread's `jmx_locked_synchronizers`
    /// mutex to `retain` the one it was removing, so the cost of one
    /// uncontended lock grew with the number of threads in the VM — a property
    /// no correct JVM has, and one that a single-threaded microbenchmark like
    /// `probes/AqsBreakdownProbe.java` cannot see at all.
    /// `probes/AqsOwnerScaleProbe.java` is the differential that does.
    ///
    /// `synchronizer_owner` is a derived mirror of the per-thread lists: this
    /// function is their only mutator besides the GC remap in
    /// `update_thread_objs_after_gc` (which rekeys the index in the same pass,
    /// because the key is a heap address and a moving collection relocates it)
    /// and thread reaping. So the previous owner can be looked up instead of
    /// searched for, and the transition touches at most two threads' lists.
    ///
    /// A stale index entry naming a thread that has since exited resolves to
    /// `None` in `threads.get` and is simply dropped — the same no-op the scan
    /// performed for a thread whose entry was already gone.
    /// This thread's own `jmx_locked_synchronizers` list, as a handle it can
    /// hold and reach directly.
    ///
    /// The same shape as [`Self::gc_block_state_of`], and for the same reason:
    /// a thread that needs its own entry on a hot path should not be taking the
    /// registry-wide `RwLock` to find itself in a map. `None` for a thread that
    /// is not registered, whose caller must then use the general
    /// [`Self::set_jmx_owned_synchronizer`].
    ///
    /// Fetch once and keep it: the entry for a given `ThreadId` is inserted
    /// exactly once (`next_id` is monotonic, so ids are never reused), so the
    /// handle cannot go stale while the thread lives. After the thread is
    /// reaped the handle keeps an orphaned list alive and nothing reads it,
    /// which is the correct outcome for a thread that is gone.
    pub fn jmx_locked_synchronizers_of(
        &self,
        thread_id: ThreadId,
    ) -> Option<Arc<Mutex<Vec<ObjectRef>>>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| Arc::clone(&entry.jmx_locked_synchronizers))
    }

    /// [`Self::set_jmx_owned_synchronizer`] for the case AQS actually performs:
    /// the calling thread is the transition's subject, and its own list is the
    /// only one that changes.
    ///
    /// `acquire` passes `Thread.currentThread()` and `release` passes `null`,
    /// both from the owning thread, so `previous` is either `None` or the caller
    /// itself on every transition a `ReentrantLock` generates. The general form
    /// reads the registry `RwLock` and hashes the owner's `ThreadId` to find the
    /// list it already knows — measured at 45.4 ns for one acquire/release pair
    /// in `jit::helpers::jit_native_dispatch_profile`, on a path that runs twice
    /// per uncontended lock/unlock pair.
    ///
    /// A genuine cross-thread steal (`previous` naming a peer) still takes the
    /// registry route for that peer's list, so the recorded state is identical
    /// either way — this only removes a lookup from the case that does not need
    /// one. `own_list` must be the handle [`Self::jmx_locked_synchronizers_of`]
    /// returned for `self_id`; passing any other list would silently split the
    /// record the JMX snapshot reads from the one this writes.
    pub fn set_jmx_owned_synchronizer_own(
        &self,
        self_id: ThreadId,
        own_list: &Mutex<Vec<ObjectRef>>,
        owner: Option<ThreadId>,
        synchronizer: ObjectRef,
    ) {
        debug_assert!(
            owner.is_none() || owner == Some(self_id),
            "set_jmx_owned_synchronizer_own is for a self-owned transition;              owner={owner:?} self={self_id:?}"
        );
        let key = synchronizer.as_ptr() as usize;
        let previous = {
            let mut index = self.synchronizer_owner.lock();
            match owner {
                Some(owner) => index.insert(key, owner),
                None => index.remove(&key),
            }
        };
        match previous {
            // Ownership was stolen from a peer without it releasing. Not a
            // shape AQS produces; keep it exact rather than fast.
            Some(previous) if previous != self_id => {
                {
                    let threads = self.threads.read();
                    if let Some(entry) = threads.get(&previous) {
                        entry
                            .jmx_locked_synchronizers
                            .lock()
                            .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                    }
                }
                if owner == Some(self_id) {
                    own_list.lock().push(synchronizer);
                }
            }
            // The caller already held it: a release clears it, and a repeated
            // acquire (a reentrant setter re-run) must not double-count.
            Some(_) => {
                if owner.is_none() {
                    own_list
                        .lock()
                        .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                }
            }
            None => {
                if owner == Some(self_id) {
                    own_list.lock().push(synchronizer);
                }
            }
        }
    }

    pub fn set_jmx_owned_synchronizer(&self, owner: Option<ThreadId>, synchronizer: ObjectRef) {
        let key = synchronizer.as_ptr() as usize;
        let previous = {
            let mut index = self.synchronizer_owner.lock();
            match owner {
                Some(owner) => index.insert(key, owner),
                None => index.remove(&key),
            }
        };
        let threads = self.threads.read();
        if let Some(previous) = previous {
            if Some(previous) != owner {
                if let Some(entry) = threads.get(&previous) {
                    entry
                        .jmx_locked_synchronizers
                        .lock()
                        .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                }
            }
        }
        if let Some(owner) = owner {
            // `previous == Some(owner)` means this synchronizer was already
            // recorded against this thread (a reentrant acquire re-running the
            // setter); pushing again would double-count it in the snapshot.
            if previous == Some(owner) {
                return;
            }
            if let Some(entry) = threads.get(&owner) {
                entry.jmx_locked_synchronizers.lock().push(synchronizer);
            }
        }
    }

    pub fn jmx_lock_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Option<(
        Option<ObjectRef>,
        Option<ObjectRef>,
        Vec<ObjectRef>,
        Vec<ObjectRef>,
        i64,
        i64,
        i64,
        i64,
    )> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        let snapshot = (
            *entry.jmx_contended_monitor.lock(),
            *entry.jmx_waiting_monitor.lock(),
            entry.jmx_locked_monitors.lock().clone(),
            entry.jmx_locked_synchronizers.lock().clone(),
            (entry.jmx_blocked_nanos.load(Ordering::Relaxed) / 1_000_000) as i64,
            entry.jmx_blocked_count.load(Ordering::Relaxed) as i64,
            (entry.jmx_waited_nanos.load(Ordering::Relaxed) / 1_000_000) as i64,
            entry.jmx_waited_count.load(Ordering::Relaxed) as i64,
        );
        Some(snapshot)
    }

    pub fn reset_jmx_contention_stats(&self) {
        for entry in self.threads.read().values() {
            entry.jmx_blocked_count.store(0, Ordering::Relaxed);
            entry.jmx_blocked_nanos.store(0, Ordering::Relaxed);
            entry.jmx_waited_count.store(0, Ordering::Relaxed);
            entry.jmx_waited_nanos.store(0, Ordering::Relaxed);
            *entry.jmx_contended_started.lock() = None;
            *entry.jmx_wait_started.lock() = None;
        }
    }

    /// Return all (ThreadId, name) pairs for currently registered threads.
    pub fn all_thread_names(&self) -> Vec<(ThreadId, String)> {
        self.threads
            .read()
            .iter()
            .map(|(tid, entry)| (*tid, entry.name.clone()))
            .collect()
    }

    /// Set the park state for a given ThreadId (used when spawning threads
    /// to share the JvmThread's ParkState with the registry).
    pub fn set_park_state(&self, thread_id: ThreadId, state: Arc<ParkState>) {
        let obj_opt;
        {
            let mut threads = self.threads.write();
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
            .read()
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
        let threads = self.threads.read();
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
        let threads = self.threads.read();
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
        let threads = self.threads.read();
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
        if let Some(entry) = self.threads.read().get(&thread_id) {
            entry
                .interrupted
                .store(value, std::sync::atomic::Ordering::Release);
        }
    }

    /// Get the interrupted flag Arc for a thread (to share with JvmThread).
    pub fn get_interrupted_flag(&self, thread_id: ThreadId) -> Option<Arc<AtomicBool>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.interrupted.clone())
    }

    /// Set the interrupted flag Arc for a thread (share JvmThread's flag with registry).
    pub fn set_interrupted_flag(&self, thread_id: ThreadId, flag: Arc<AtomicBool>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.interrupted = flag;
        }
    }

    /// Set the root snapshot Arc for a thread (share JvmThread's snapshot with registry).
    pub fn set_root_snapshot(&self, thread_id: ThreadId, snapshot: Arc<Mutex<Vec<ObjectRef>>>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
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
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.frame_trace = trace;
        }
    }

    /// Share the JvmThread's VM-state breadcrumb with the registry.
    pub fn set_vm_state(&self, thread_id: ThreadId, state: Arc<Mutex<String>>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.vm_state = state;
        }
    }

    /// Read a copy of `thread_id`'s last-published frame trace (call stack),
    /// innermost frame first. Empty if the thread is unknown or never deposited.
    ///
    /// **Every entry has `line_number == -1`.** The depositor
    /// (`stackwalker::capture_frames_no_lines`, called from every blocking
    /// deposit point) is deliberately lock-free and takes no `ClassStore`
    /// borrow, so it cannot resolve lines. Prefer
    /// [`frame_trace_of_resolved`](Self::frame_trace_of_resolved) at any call
    /// site that has a `ClassStore` in hand — a thread dump then gains source
    /// lines it does not have today, at zero cost to the deposit path.
    pub fn frame_trace_of(&self, thread_id: ThreadId) -> Vec<cratonvm_native_api::StackTraceEntry> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.frame_trace.lock().clone())
            .unwrap_or_default()
    }

    /// [`frame_trace_of`](Self::frame_trace_of) with source line numbers filled
    /// in from `class_store`.
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-SW-2 of
    /// `arch-2026-07-26/stackwalk-and-vtable.md`). The published
    /// snapshot is line-less because the *depositor* must stay lock-free; the
    /// *reader* usually does hold a `ClassStore` (cross-thread
    /// `Thread.getStackTrace()`, `dumpThreads()`, the JMX thread dump), so it
    /// can pay for the resolution once, only when a dump is actually taken.
    ///
    /// Strictly additive: `stackwalker::resolve_line_numbers_in_place` is
    /// fail-closed — it yields the line an eager capture would have produced or
    /// leaves `-1` — so this can only replace unknowns with correct lines. It
    /// never produces a wrong line, and it never touches an entry that already
    /// has one (including the `-2` native sentinel).
    ///
    /// Resolution is exact for frames carrying `StackTraceEntry::method_index`;
    /// the deposit path cannot compute one (it has no `ClassStore`), so those
    /// frames fall back to the unambiguous-name rule and overloaded frames stay
    /// at `-1`. That is still strictly more than the all-`-1` snapshot.
    ///
    /// The registry lock is released before the resolution runs: the walk over
    /// the returned `Vec` touches no registry state, and holding L5 across a
    /// `ClassStore` walk would invert the usual acquisition order.
    pub fn frame_trace_of_resolved(
        &self,
        thread_id: ThreadId,
        class_store: &crate::classloading::ClassStore,
    ) -> Vec<cratonvm_native_api::StackTraceEntry> {
        let mut trace = self.frame_trace_of(thread_id);
        crate::runtime::stackwalker::resolve_line_numbers_in_place(class_store, &mut trace);
        trace
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
        let threads = self.threads.read();
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
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.root_snapshot.lock().clear();
            entry.frame_trace.lock().clear();
            entry
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }

    /// Whether a live registered thread is in a GC-safe blocking region.
    ///
    /// The same transition is the authoritative source for Java
    /// `Thread.State.WAITING`: it is published before monitor/AQS/native waits
    /// and cleared only after the thread wakes and applies post-GC fixups.
    pub fn is_blocked(&self, thread_id: ThreadId) -> bool {
        let threads = self.threads.read();
        threads.get(&thread_id).is_some_and(|entry| {
            entry.alive.load(Ordering::Acquire)
                && entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire)
        })
    }

    /// The Java blocking state for a currently blocked thread: 1 is WAITING,
    /// 2 is BLOCKED, 3 is TIMED_WAITING. Returns 0 for running, dead, and
    /// unknown threads.
    pub fn java_block_state(&self, thread_id: ThreadId) -> u8 {
        let threads = self.threads.read();
        threads
            .get(&thread_id)
            .filter(|entry| {
                entry.alive.load(Ordering::Acquire)
                    && entry
                        .gc_block_state
                        .in_blocked_region
                        .load(Ordering::Acquire)
            })
            .map(|entry| entry.gc_block_state.java_state.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Clear the GC-blocked mark for a VM-registered native carrier thread.
    pub fn mark_native_thread_unblocked(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            {
                let mut f = entry.gc_block_state.fixup.lock();
                if !f.is_empty()
                    && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some()
                {
                    eprintln!(
                        "[blockgc] native-unblock DISCARDS {} fixups tid={}",
                        f.len(),
                        thread_id.0,
                    );
                }
                f.clear();
            }
            entry.gc_block_state.slot_origins.lock().clear();
            entry.root_snapshot.lock().clear();
            entry
                .gc_block_state
                .in_blocked_region
                .store(false, Ordering::Release);
        }
    }

    /// CENSUS-RECONCILE — the shared [`GcBlockState`] published for
    /// `thread_id`.
    ///
    /// This is the SAME `Arc` the owning `JvmThread` holds (installed by
    /// [`Self::set_gc_block_state`]), so a caller that cannot reach the
    /// `JvmThread` itself can still hand the *authoritative*
    /// `in_blocked_region` flag to
    /// [`crate::threading::gc_barrier::GcBarrier::leave_blocked_region_flagged`]
    /// rather than clearing it with a bare `store(false)` (finding 1(c): a
    /// bare store lets a pause requested between the drain and the store both
    /// EXCLUDE the thread and let it run).
    ///
    /// `jni::host_thread_leave_native` is the motivating caller: it runs on a
    /// host OS thread that has no `JNI_THREAD` binding, so `with_jni_context`
    /// — every other path's route to the flag — resolves to `None` there.
    ///
    /// The `Arc` is CLONED out rather than borrowed on purpose: the registry
    /// lock must be released before the caller can park inside the barrier.
    /// Holding it across a pause drain would queue every later census behind a
    /// pending registry writer.
    pub fn gc_block_state_of(&self, thread_id: ThreadId) -> Option<Arc<GcBlockState>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.gc_block_state.clone())
    }

    /// CENSUS-RECONCILE — the registered, alive `ThreadId` whose published
    /// `os_tid` is the CALLING OS thread's.
    ///
    /// The identity census (`alive_count_blocked_and_os_tids`) keys the STW
    /// exclusion set on `ThreadId`, so a host-facing entry point that wants to
    /// be *excluded* must name itself. A host thread parked outside the VM has
    /// no `JvmThread` borrow and no JNI TLS binding, but its carrier's
    /// `os_tid` was published by [`Self::set_os_tid_current`] at registration
    /// — which makes the OS id the only identity link that survives leaving
    /// the VM.
    ///
    /// Returns `None` when the answer would be ambiguous or unknown:
    ///
    /// * no alive entry claims this OS thread (a genuinely foreign host
    ///   thread — it is not in `alive_count` either, so it occupies no
    ///   `expected` slot and needs no exclusion),
    /// * MORE than one does (a virtual thread mounted on this carrier
    ///   republishes the carrier's `os_tid` under the vthread's own id, see
    ///   `vm_exec.rs`'s mount path — guessing between them could exclude the
    ///   wrong identity from a pause), or
    /// * the platform has no `os_tid` backend at all (`current_os_tid() == 0`).
    ///
    /// `None` is always the safe answer: the caller then falls back to the
    /// anonymous-counter-only behaviour, which is what every such call did
    /// before this existed.
    pub fn thread_id_for_current_os_tid(&self) -> Option<ThreadId> {
        let os_tid = current_os_tid();
        if os_tid == 0 {
            return None;
        }
        let threads = self.threads.read();
        let mut found: Option<ThreadId> = None;
        for (tid, e) in threads.iter() {
            if e.alive.load(Ordering::Acquire) && e.os_tid.load(Ordering::Acquire) == os_tid {
                if found.is_some() {
                    return None; // ambiguous — see the doc comment
                }
                found = Some(*tid);
            }
        }
        found
    }

    /// DBG (CRATONVM_DBG_MTROOTS): per-thread (tid, in_blocked_region,
    /// snapshot_len) for every alive thread. Used at an STW to see whether a
    /// thread that holds a reclaimed live oop was counted BLOCKED (excluded from
    /// the barrier `expected`) while actually running — the multi-thread
    /// root-coverage gap.
    pub fn dump_blocked_states(&self) -> Vec<(u64, bool, usize)> {
        let threads = self.threads.read();
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

        let threads = self.threads.read();
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
            // Same `deposit=` tag as the T19.H1 summary, for the same reason:
            // this census is alive-only, but "alive" does not mean "parked at
            // the frames below" — a mutator the barrier is still waiting on is
            // by definition RUNNING, and its chain is last-block history.
            // See `ThreadRegistry::deposit_freshness`.
            let _ = write!(
                out,
                "\n  t{} os_tid={} name={:?} blocked={} ready={} snapshot={} state={:?} deposit={} top={}",
                tid.0,
                os_tid,
                entry.name,
                blocked,
                stw_ready,
                snapshot_len,
                vm_state,
                Self::deposit_freshness(true, blocked),
                top
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
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("collect_all_root_snapshots");
        let len = self.threads.read().len();
        PERF.time(len, || self.collect_all_root_snapshots_inner())
    }

    fn collect_all_root_snapshots_inner(&self) -> Vec<ObjectRef> {
        let threads = self.threads.read();
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
                if let Some(obj) = *entry.jmx_contended_monitor.lock() {
                    all_roots.push(obj);
                }
                if let Some(obj) = *entry.jmx_waiting_monitor.lock() {
                    all_roots.push(obj);
                }
                all_roots.extend(entry.jmx_locked_monitors.lock().iter().copied());
                all_roots.extend(entry.jmx_locked_synchronizers.lock().iter().copied());
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
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
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
    pub fn update_thread_objs_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        let mut threads = self.threads.write();
        let mut rekeyed: Vec<(usize, usize)> = Vec::new();
        // Vacated mirror addresses + owning tid, recorded into
        // `former_mirror_addrs` AFTER `threads` is dropped (lock order).
        let mut vacated: Vec<(usize, ThreadId)> = Vec::new();
        // `(old_addr, new_addr, owner)` for every recorded owned synchronizer
        // this collection relocated; applied to `synchronizer_owner` after
        // `threads` is dropped, same lock discipline as `vacated`.
        let mut synchronizer_rekeys: Vec<(usize, usize, ThreadId)> = Vec::new();
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
            let mut remap_jmx = |obj: &mut ObjectRef| {
                if let Some(&new_addr) = pointer_map.get(&(obj.as_ptr() as usize)) {
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            };
            if let Some(obj) = entry.jmx_contended_monitor.lock().as_mut() {
                remap_jmx(obj);
            }
            if let Some(obj) = entry.jmx_waiting_monitor.lock().as_mut() {
                remap_jmx(obj);
            }
            for obj in entry.jmx_locked_monitors.lock().iter_mut() {
                remap_jmx(obj);
            }
            // `synchronizer_owner` is keyed by these very addresses, so the
            // rekeying happens in the same pass that rewrites them: a key left
            // pointing at a vacated address would make the next
            // `set_jmx_owned_synchronizer` for that lock miss its previous
            // owner and record it against two threads at once. The `(old, new,
            // owner)` triples are collected here and applied after `threads` is
            // dropped, keeping this function's "acquire, use, drop, then call
            // out" lock discipline.
            for obj in entry.jmx_locked_synchronizers.lock().iter_mut() {
                let old_addr = obj.as_ptr() as usize;
                remap_jmx(obj);
                let new_addr = obj.as_ptr() as usize;
                if new_addr != old_addr {
                    synchronizer_rekeys.push((old_addr, new_addr, *tid));
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
        if !synchronizer_rekeys.is_empty() {
            let mut index = self.synchronizer_owner.lock();
            for (old_addr, new_addr, tid) in synchronizer_rekeys {
                // Only move an entry this index actually owns AND that still
                // names the thread whose list we just remapped: a synchronizer
                // recorded against someone else is that owner's row to move,
                // and it will be moved by its own iteration of the loop above.
                if index.get(&old_addr) == Some(&tid) {
                    index.remove(&old_addr);
                    index.insert(new_addr, tid);
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
    pub fn fold_pointer_map_into_blocked(&self, pointer_map: &cratonvm_types::PointerMap) {
        self.fold_pointer_map_into_blocked_audited(pointer_map, None)
    }

    /// [`Self::fold_pointer_map_into_blocked`] plus the post-fold invariant
    /// check, for the production call site that can hand over the heap.
    ///
    /// The invariant: once this fold returns, no address a blocked thread will
    /// resume on may lie in the INACTIVE young semispace. That arena is what
    /// the moving cycle just evacuated and zeroed, so a reference into it is a
    /// still-referenced object the collector took — the "all-zero header
    /// `java.lang.Object` receiver" family — and it is detectable HERE, in the
    /// cycle that caused it, with the frame and slot that hold it. Every other
    /// witness of this bug is downstream: a `NoSuchMethodError` against
    /// `java.lang.Object`, a `checkcast` failure, an out-of-bounds field read —
    /// each an unbounded distance from the collection with the gap, and each
    /// naming only the victim's *use* site.
    ///
    /// The check that runs UNCONDITIONALLY is the precise one: a
    /// `slot_origins` entry — the exact `(frame, slot)` tracker, filled by the
    /// blocking deposit and advanced through each cycle's pointer map — whose
    /// `cur` lands in the vacated arena. Its report carries
    /// `was_a_scanned_root`, which forks the fix: `false` means the deposit
    /// never published that slot (a root COVERAGE gap), `true` means the
    /// collector was handed the address and left it behind (an EVACUATION
    /// gap).
    ///
    /// The whole-snapshot version of the same question is behind
    /// `CRATONVM_DBG_BLOCKGC`, because a snapshot legitimately carries
    /// conservative candidates that are not object starts and which
    /// `forward_object` correctly declines to relocate.
    ///
    /// `heap` is `None` only from tests that drive the fold directly with a
    /// synthetic pointer map.
    pub fn fold_pointer_map_into_blocked_audited(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        heap: Option<&crate::memory::VmHeap>,
    ) {
        if pointer_map.is_empty() {
            return;
        }
        let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some();
        // One arena lock for the whole fold; the per-address test below is two
        // integer compares, so the audit is affordable unconditionally and does
        // not need a flag to have been set before the run that reproduces.
        let vacated = heap.and_then(|h| h.young_inactive_semispace_range());
        let threads = self.threads.read();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            if !entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire)
            {
                // NOT blocked: the fixup chain and `slot_origins` below are
                // blocked-region machinery and do not apply.
                //
                // A snapshot remap was tried here on 2026-08-17 — on the theory
                // that a thread censused as blocked and then leaving the region
                // is skipped by this fold AND not waited for by the pause, so
                // its stale snapshot is what the next collection marks from.
                // The theory is sound and the change is a no-op:
                // `CRATONVM_DBG_ROOT_REMAP_AUDIT=1`, which re-runs the root scan
                // at the END of `update_all_roots` and looks for an address this
                // collection moved (excluding slide destinations), reports ZERO
                // with the remap and ZERO without it, on the workload that
                // reproduces the H2 MVStore-writer residual. Every stale
                // snapshot entry it did find was on a BLOCKED thread and was
                // already handled below.
                //
                // Left unwritten deliberately: an unmeasured behaviour change
                // in the root set is exactly what this file's history is made
                // of. If the excluded-thread race is ever observed, the audit
                // above is how to see it and this is where the fix goes.
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
            // cceres3 FIX: advance the exact per-slot tracker through THIS
            // collection's pointer map (see `GcBlockState::slot_origins`).
            // Exact lookups per map — no chain keys to strand.
            {
                let mut origins = entry.gc_block_state.slot_origins.lock();
                for so in origins.iter_mut() {
                    if let Some(&new) = pointer_map.get(&so.cur) {
                        so.cur = new;
                    }
                }
                if let Some((lo, hi)) = vacated {
                    // Built lazily: a frame slot landing in the vacated arena
                    // is the rare case, and this set is only needed to report
                    // one.
                    let mut in_snapshot: Option<rustc_hash::FxHashSet<usize>> = None;
                    for so in origins.iter() {
                        // A DEAD local in the vacated arena is the per-bci
                        // liveness analysis working as designed, not a defect
                        // — see `SlotOrigin::live`.
                        if !so.live || so.cur < lo || so.cur >= hi {
                            continue;
                        }
                        let published = in_snapshot
                            .get_or_insert_with(|| {
                                snapshot.iter().map(|r| r.as_ptr() as usize).collect()
                            })
                            .contains(&so.cur);
                        // The remap above rewrote the snapshot in place, so a
                        // hit means the collector had this exact address as a
                        // root and left it behind; a miss means the deposit
                        // never published it.
                        blocked_root_gap_report(
                            tid.0, so.frame, so.idx, so.is_stack, so.orig, so.cur, published,
                        );
                    }
                }
            }
            // The whole-snapshot version of the same question. Gated, unlike
            // the per-slot check above, because the snapshot legitimately
            // carries CONSERVATIVE candidates — register/stack words and
            // JIT-band scans that are not object starts — and `forward_object`
            // correctly declines to relocate those, so they land in the vacated
            // arena on every moving cycle by design. Only a hit that is ALSO a
            // frame slot is unambiguous, and that is what the check above
            // reports unconditionally.
            if dbg {
                if let Some((lo, hi)) = vacated {
                    for r in snapshot.iter() {
                        let a = r.as_ptr() as usize;
                        if a >= lo && a < hi {
                            blocked_snapshot_gap_report(tid.0, a);
                        }
                    }
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
        self.threads.read().len()
    }

    /// Get the number of alive threads.
    pub fn alive_count(&self) -> usize {
        self.threads
            .read()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .count()
    }

    /// xt-hardening (2026-07-03): publish the calling thread's OS thread id
    /// for `thread_id` (see `ThreadEntry::os_tid`). Called by the thread
    /// itself at startup, before it can execute any Java/JIT code.
    pub fn set_os_tid_current(&self, thread_id: ThreadId) {
        // CRATONVM_DBG_BLOCKED_ACCESS: register THIS thread's authoritative
        // `in_blocked_region` flag with the gc crate's heap-access canary,
        // which cannot reach the `JvmThread` from the `get_header` funnel.
        // Like the os_tid publish itself, this always runs on the owning
        // thread at startup (main / spawned / native-carrier). No-op when the
        // gate is off. The leaked `Arc<GcBlockState>` clone (debug-gated, one
        // per thread) pins the flag's address for the process lifetime, which
        // is the safety contract `register_self_blocked_flag` requires.
        if cratonvm_gc::blocked_access_debug::enabled() {
            let threads = self.threads.read();
            if let Some(entry) = threads.get(&thread_id) {
                let keep = entry.gc_block_state.clone();
                let flag: *const std::sync::atomic::AtomicBool = &keep.in_blocked_region;
                std::mem::forget(keep);
                // SAFETY: `flag` points into the leaked Arc's referent above,
                // so it stays valid for the process lifetime; only the owning
                // thread reads it back through its TLS slot.
                unsafe { cratonvm_gc::blocked_access_debug::register_self_blocked_flag(flag) };
            }
        }
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetCurrentThreadId() -> u32;
            }
            let os_tid = unsafe { GetCurrentThreadId() };
            let threads = self.threads.read();
            if let Some(entry) = threads.get(&thread_id) {
                entry.os_tid.store(os_tid, Ordering::Release);
            }
        }
        #[cfg(target_os = "linux")]
        {
            let os_tid = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
            let threads = self.threads.read();
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
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_and_os_tids");
        let len = self.threads.read().len();
        PERF.time(len, || self.alive_count_and_os_tids_inner())
    }

    fn alive_count_and_os_tids_inner(&self) -> (usize, Vec<u32>) {
        let threads = self.threads.read();
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
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_blocked_and_os_tids");
        let len = self.threads.read().len();
        PERF.time(len, || self.alive_count_blocked_and_os_tids_inner())
    }

    fn alive_count_blocked_and_os_tids_inner(&self) -> (usize, usize, Vec<u32>, Vec<u64>) {
        let threads = self.threads.read();
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
        let threads = self.threads.read();
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
            .read()
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
            .read()
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

/// Rate limit for the two blocked-root-gap reporters below. The defect
/// cascades — one blocked thread typically has many slots pointing at the
/// same lost subgraph — and the first handful carry all the information.
const BLOCKED_ROOT_GAP_REPORTS: u64 = 12;

/// A blocked thread's frame slot still points into the arena this moving
/// cycle evacuated: the object it names was NOT relocated, so on wake the
/// slot reads the all-zero header the collector left behind.
///
/// `in_snapshot` is the fork that decides where the fix goes. `false` (the
/// common case) means `deposit_root_snapshot`'s frame scan never published
/// this slot, so the collector could not have known to evacuate it — a
/// root-coverage gap in the deposit. `true` means the collector held this
/// exact address as a root and left it behind anyway — an evacuation gap.
fn blocked_root_gap_report(
    tid: u64,
    frame: u32,
    idx: u32,
    is_stack: bool,
    orig: usize,
    cur: usize,
    in_snapshot: bool,
) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, Ordering::Relaxed) >= BLOCKED_ROOT_GAP_REPORTS {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        tid = tid,
        frame = frame,
        slot = if is_stack { "stack" } else { "local" },
        idx = idx,
        deposited_addr = format!("{orig:#x}"),
        current_addr = format!("{cur:#x}"),
        was_a_scanned_root = in_snapshot,
        "blocked-thread frame slot points into the semispace this moving cycle just \
         evacuated — the object was not relocated and the slot will read an all-zero \
         `java.lang.Object` header on wake. `was_a_scanned_root=false` means the \
         blocking deposit never published this slot (root-coverage gap); `true` means \
         the collector was handed it and did not evacuate it (evacuation gap).",
    );
}

/// A blocked thread's deposited root snapshot still names the evacuated
/// arena after the fold remapped it.
///
/// `CRATONVM_DBG_BLOCKGC` only. Most hits are benign by construction: the
/// snapshot carries conservative candidates (register/stack words, JIT-band
/// scans) that are not object starts, and `forward_object` declines to
/// relocate anything `young_object_starts` does not vouch for. A hit is only
/// interesting when the SAME address is also a frame slot — which
/// [`blocked_root_gap_report`] reports on its own, unconditionally.
fn blocked_snapshot_gap_report(tid: u64, addr: usize) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, Ordering::Relaxed) >= BLOCKED_ROOT_GAP_REPORTS {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        tid = tid,
        obj = format!("{addr:#x}"),
        "blocked-thread root snapshot still names the semispace this moving cycle just \
         evacuated, after the fold's remap — the collector scanned this root and did not \
         relocate it. The next collection would scan the same stale address.",
    );
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ThreadRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let threads = self.threads.read();
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

    /// Fabricated, never-dereferenced heap addresses. Everything the owned-
    /// synchronizer index does with an `ObjectRef` is take `as_ptr()` and use
    /// it as a map key / identity comparison, so these never need to be real
    /// objects — and making them real would mean booting a heap for a test
    /// about bookkeeping.
    fn fake_synchronizer(addr: usize) -> ObjectRef {
        // SAFETY: this ref is only ever compared and hashed by address; no
        // code under test reads through it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn owned_synchronizers(registry: &ThreadRegistry, tid: ThreadId) -> Vec<usize> {
        registry
            .jmx_lock_snapshot(tid)
            .map(|snapshot| snapshot.3.iter().map(|o| o.as_ptr() as usize).collect())
            .unwrap_or_default()
    }

    /// [`ThreadRegistry::set_jmx_owned_synchronizer_own`] must record exactly
    /// what the general form records, for **every** (previous owner, new owner)
    /// combination — not only the two AQS actually generates.
    ///
    /// The fast form exists because the general one reads the registry-wide
    /// `RwLock` to find a list the calling thread could have held a handle to.
    /// That is only sound if the two are indistinguishable in their effect, and
    /// the interesting combinations are the ones AQS does *not* produce: a
    /// steal from a peer, and a release of a lock a peer owns. Both take the
    /// fast form's fallback arm, and a divergence there would surface as a
    /// `ThreadInfo.getLockedSynchronizers()` that names two owners for one lock
    /// — silently, and only under JMX inspection.
    ///
    /// Driven as a table so a future arm cannot be added without a case.
    #[test]
    fn the_own_handle_transition_records_what_the_general_one_records() {
        // (label, previous owner, new owner)
        let cases: [(&str, Option<ThreadId>, Option<ThreadId>); 6] = [
            ("first acquire by self", None, Some(ThreadId(1))),
            ("reentrant acquire by self", Some(ThreadId(1)), Some(ThreadId(1))),
            ("release by self", Some(ThreadId(1)), None),
            ("steal from a peer", Some(ThreadId(2)), Some(ThreadId(1))),
            ("release of a peer's lock", Some(ThreadId(2)), None),
            ("release of an unowned lock", None, None),
        ];
        let (me, peer) = (ThreadId(1), ThreadId(2));
        let lock = fake_synchronizer(0x2000);

        for (label, previous, owner) in cases {
            // Two registries, seeded identically, then driven one call apart.
            let general = ThreadRegistry::new();
            let own = ThreadRegistry::new();
            for registry in [&general, &own] {
                registry.register(me, "me", None);
                registry.register(peer, "peer", None);
                if let Some(previous) = previous {
                    registry.set_jmx_owned_synchronizer(Some(previous), lock);
                }
            }

            general.set_jmx_owned_synchronizer(owner, lock);
            let own_list = own
                .jmx_locked_synchronizers_of(me)
                .expect("the handle must exist for a registered thread");
            own.set_jmx_owned_synchronizer_own(me, &own_list, owner, lock);

            for tid in [me, peer] {
                assert_eq!(
                    owned_synchronizers(&general, tid),
                    owned_synchronizers(&own, tid),
                    "{label}: the two forms disagree about what {tid:?} owns"
                );
            }
        }
    }

    /// The handle must be the *same* list the JMX snapshot reads.
    ///
    /// This is the one way the fast form can be wrong without any of its own
    /// logic being wrong: hand it a list that is not the registry's, and every
    /// write lands somewhere `jmx_lock_snapshot` will never look. The `Arc` is
    /// what makes them one list, and nothing else in the type system says so.
    #[test]
    fn the_handle_and_the_snapshot_are_the_same_list() {
        let registry = ThreadRegistry::new();
        let me = ThreadId(1);
        registry.register(me, "me", None);
        let handle = registry.jmx_locked_synchronizers_of(me).expect("registered");
        handle.lock().push(fake_synchronizer(0x3000));
        assert_eq!(
            owned_synchronizers(&registry, me),
            vec![0x3000],
            "a write through the handle was invisible to jmx_lock_snapshot — \
             the handle is not the registry's list"
        );
    }

    /// An unregistered thread has no handle, which is what sends its caller to
    /// the general form rather than to a private list nobody reads.
    #[test]
    fn an_unregistered_thread_has_no_synchronizer_handle() {
        let registry = ThreadRegistry::new();
        assert!(registry.jmx_locked_synchronizers_of(ThreadId(77)).is_none());
    }

    /// An AQS ownership transition must leave the synchronizer recorded against
    /// exactly one thread — the property the old Θ(threads) scan bought by
    /// walking every thread and taking every thread's mutex, twice per
    /// uncontended `ReentrantLock.lock()`/`unlock()` pair.
    ///
    /// The reverse index replaces the scan; this pins that it still answers the
    /// same. `probes/AqsOwnerScaleProbe.java` is the companion measurement —
    /// with the scan in place, an uncontended lock got ~2x more expensive going
    /// from 1 to 256 live threads while a `synchronized` control stayed flat.
    #[test]
    fn an_ownership_transition_records_the_synchronizer_against_exactly_one_thread() {
        let registry = ThreadRegistry::new();
        let (a, b, c) = (ThreadId(1), ThreadId(2), ThreadId(3));
        for (tid, name) in [(a, "a"), (b, "b"), (c, "c")] {
            registry.register(tid, name, None);
        }
        let lock = fake_synchronizer(0x1000);

        // acquire on A
        registry.set_jmx_owned_synchronizer(Some(a), lock);
        assert_eq!(owned_synchronizers(&registry, a), vec![0x1000]);
        assert!(owned_synchronizers(&registry, b).is_empty());

        // A reentrant acquire re-runs the setter; it must not double-record.
        registry.set_jmx_owned_synchronizer(Some(a), lock);
        assert_eq!(owned_synchronizers(&registry, a), vec![0x1000]);

        // Hand-off to B without an intervening release: A must lose it.
        registry.set_jmx_owned_synchronizer(Some(b), lock);
        assert!(
            owned_synchronizers(&registry, a).is_empty(),
            "the former owner must not keep a lock it no longer holds"
        );
        assert_eq!(owned_synchronizers(&registry, b), vec![0x1000]);

        // release
        registry.set_jmx_owned_synchronizer(None, lock);
        for tid in [a, b, c] {
            assert!(
                owned_synchronizers(&registry, tid).is_empty(),
                "a released synchronizer is owned by nobody"
            );
        }

        // A second, distinct lock is independent of the first.
        let other = fake_synchronizer(0x2000);
        registry.set_jmx_owned_synchronizer(Some(c), lock);
        registry.set_jmx_owned_synchronizer(Some(c), other);
        let mut held = owned_synchronizers(&registry, c);
        held.sort_unstable();
        assert_eq!(held, vec![0x1000, 0x2000]);
        registry.set_jmx_owned_synchronizer(None, lock);
        assert_eq!(owned_synchronizers(&registry, c), vec![0x2000]);
    }

    /// The index is keyed by a heap address, so a moving collection that
    /// relocates a held synchronizer has to rekey it in the same pass that
    /// remaps the per-thread lists. Without that, the next transition for the
    /// lock misses its previous owner and records it against two threads.
    #[test]
    fn a_moving_collection_rekeys_the_owned_synchronizer_index() {
        let registry = ThreadRegistry::new();
        let (a, b) = (ThreadId(1), ThreadId(2));
        registry.register(a, "a", None);
        registry.register(b, "b", None);

        registry.set_jmx_owned_synchronizer(Some(a), fake_synchronizer(0x1000));
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0x1000usize, 0x9000usize);
        registry.update_thread_objs_after_gc(&pointer_map);

        assert_eq!(
            owned_synchronizers(&registry, a),
            vec![0x9000],
            "the list itself is remapped"
        );

        // Now hand the (relocated) lock to B. If the index still held the
        // vacated key, A would keep its stale entry and both threads would
        // report owning it.
        registry.set_jmx_owned_synchronizer(Some(b), fake_synchronizer(0x9000));
        assert!(
            owned_synchronizers(&registry, a).is_empty(),
            "the index followed the object, so the former owner was found"
        );
        assert_eq!(owned_synchronizers(&registry, b), vec![0x9000]);
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

    /// T19.H1 summary honesty. The deposited frame chain is only a readout of
    /// where a thread *is* while that thread is inside a blocked region;
    /// otherwise it is history that nothing clears. An unmarked stale chain
    /// has now produced three wrong root causes (see
    /// `ThreadRegistry::deposit_freshness`), the most recent being
    /// `bug-h2-teststringcache-thread-join-blocked-on-dead-threads-20260807`:
    /// `main` was running `TestStringCache.runBenchmark()` while its untagged
    /// deposit still read `java/lang/Thread.join@129`, and the whole doc was
    /// written against that line. So the tags are asserted, not just the
    /// columns.
    #[test]
    fn t19_summary_tags_a_running_threads_deposit_as_stale() {
        assert_eq!(ThreadRegistry::deposit_freshness(true, true), "live");
        assert_eq!(ThreadRegistry::deposit_freshness(true, false), "STALE");
        assert_eq!(
            ThreadRegistry::deposit_freshness(false, true),
            "post-mortem",
            "a dead thread's raised in_blocked_region is termination residue"
        );
        assert_eq!(
            ThreadRegistry::deposit_freshness(false, false),
            "post-mortem"
        );

        let registry = ThreadRegistry::new();
        let running = ThreadId(1);
        let parked = ThreadId(2);
        let dead = ThreadId(3);
        for (tid, name) in [(running, "running"), (parked, "parked"), (dead, "dead")] {
            registry.register(tid, name, None);
            registry.set_frame_trace(
                tid,
                Arc::new(Mutex::new(vec![cratonvm_native_api::StackTraceEntry {
                    class_name: Arc::from("java/lang/Thread"),
                    method_name: Arc::from("join"),
                    source_file: Some(Arc::from("Thread.java")),
                    line_number: crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
                    byte_code_index: 129,
                    class_id: None,
                    method_index: None,
                }])),
            );
        }
        // `parked` is genuinely inside a blocked region. `dead` carries the
        // flag its exit sequence's final `deposit_root_snapshot()` raised and
        // that nothing lowers — the exact shape the H2 dump showed for its
        // three finished workers.
        for tid in [parked, dead] {
            registry
                .gc_block_state_of(tid)
                .expect("registered")
                .in_blocked_region
                .store(true, Ordering::Release);
        }
        registry.mark_dead(dead);

        let text = registry.render_thread_summary();
        let line_for = |name: &str| -> String {
            text.lines()
                .find(|l| l.contains(&format!("name={name:?}")))
                .unwrap_or_else(|| panic!("no summary row for {name}:\n{text}"))
                .to_string()
        };

        let running_line = line_for("running");
        assert!(
            running_line.contains("alive=true")
                && running_line.contains("blocked=false")
                && running_line.contains("deposit=STALE"),
            "a running thread's deposit must be tagged STALE: {running_line}"
        );
        assert!(
            line_for("parked").contains("deposit=live"),
            "a thread inside a blocked region IS at its deposited wait site"
        );
        let dead_line = line_for("dead");
        assert!(
            dead_line.contains("alive=false")
                && dead_line.contains("blocked=true")
                && dead_line.contains("deposit=post-mortem"),
            "a dead entry's raised blocked= must be labelled residue: {dead_line}"
        );

        assert!(
            text.contains("(1 at a live wait site, 1 STALE)"),
            "the chain section must split live wait sites from stale history:\n{text}"
        );
        let stale_header = text
            .lines()
            .find(|l| l.contains("name=\"running\"") && l.contains("frame(s), oldest first"))
            .unwrap_or_else(|| panic!("no chain header for the running thread:\n{text}"));
        assert!(
            stale_header.contains("deposit=STALE") && stale_header.contains("RUNNING"),
            "the stale chain's own header must say so: {stale_header}"
        );
        assert!(
            text.contains(ThreadRegistry::DEPOSIT_LEGEND),
            "the legend explaining deposit= must accompany the table:\n{text}"
        );
    }

    /// A RUNNING thread that produced no live dump must be labelled as such,
    /// and must NOT be pointed at a `"T19.H1 stack dump: tid=N"` section that
    /// was never emitted.
    ///
    /// That dangling pointer is what
    /// `known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter`
    /// was written around: the summary said "its real position is in the live
    /// stack dump above", the live dump for that thread did not exist because
    /// the thread was in JIT-compiled code, and the missing section read as
    /// "blocked somewhere the watchdog cannot reach". The thread was neither
    /// blocked nor in native code — it was spinning in a compiled
    /// `ByteBuf.writeByte` loop, and `--nojit` dumped all 73 frames.
    #[test]
    fn t19_summary_names_a_running_thread_that_produced_no_live_dump() {
        let registry = ThreadRegistry::new();
        let dumped = ThreadId(1);
        let silent = ThreadId(2);
        for (tid, name) in [(dumped, "dumped"), (silent, "silent")] {
            registry.register(tid, name, None);
            registry.set_frame_trace(
                tid,
                Arc::new(Mutex::new(vec![cratonvm_native_api::StackTraceEntry {
                    class_name: Arc::from("java/lang/Thread"),
                    method_name: Arc::from("join"),
                    source_file: Some(Arc::from("Thread.java")),
                    line_number: crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
                    byte_code_index: 129,
                    class_id: None,
                    method_index: None,
                }])),
            );
        }

        // Only tid=1 answered the watchdog.
        let text = registry.render_thread_summary_for(&[dumped.0]);
        let row_for = |name: &str| -> String {
            let lines: Vec<&str> = text.lines().collect();
            let idx = lines
                .iter()
                .position(|l| l.contains(&format!("name={name:?}")) && l.contains("alive="))
                .unwrap_or_else(|| panic!("no summary row for {name}:\n{text}"));
            lines[idx..(idx + 2).min(lines.len())].join("\n")
        };

        assert!(
            !row_for("dumped").contains("no-live-dump"),
            "a thread that DID dump must not be flagged: {}",
            row_for("dumped")
        );
        assert!(
            row_for("silent").contains("no-live-dump"),
            "a RUNNING thread that produced no dump must say so: {}",
            row_for("silent")
        );
        assert!(
            row_for("silent").contains("--nojit"),
            "and must name the one flag that separates compiled code from \
             native code: {}",
            row_for("silent")
        );

        // The chain header for the silent thread must not send the reader to a
        // section that does not exist.
        let silent_chain = text
            .lines()
            .find(|l| l.contains("name=\"silent\"") && l.contains("frame(s), oldest first"))
            .unwrap_or_else(|| panic!("no chain header for the silent thread:\n{text}"));
        assert!(
            !silent_chain.contains("stack dump: tid=2"),
            "must not point at a live section that was never emitted: {silent_chain}"
        );
        assert!(
            silent_chain.contains("no-live-dump"),
            "the silent thread's chain header must carry the tag: {silent_chain}"
        );
        let dumped_chain = text
            .lines()
            .find(|l| l.contains("name=\"dumped\"") && l.contains("frame(s), oldest first"))
            .unwrap_or_else(|| panic!("no chain header for the dumped thread:\n{text}"));
        assert!(
            dumped_chain.contains("stack dump: tid=1"),
            "a thread that DID dump keeps the pointer to its live section: {dumped_chain}"
        );

        // With no ack information at all, every running thread is reported the
        // safe way rather than the confident-and-wrong way.
        let blind = registry.render_thread_summary();
        assert_eq!(
            blind.matches("no-live-dump").count() >= 2,
            true,
            "an empty ack set must not license the 'read the dump above' \
             phrasing for anyone:\n{blind}"
        );
    }

    #[test]
    fn terminated_entries_are_bounded() {
        let registry = ThreadRegistry::new();
        for id in 1..=(TERMINATED_THREAD_ENTRY_CAP as u64 + 3) {
            let tid = ThreadId(id);
            registry.register(tid, "short-lived", None);
            registry.mark_dead(tid);
        }

        assert_eq!(registry.alive_count(), 0);
        assert_eq!(registry.count(), TERMINATED_THREAD_ENTRY_CAP);
        assert!(!registry.is_alive(ThreadId(1)));
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

    // -----------------------------------------------------------------------
    // ARCH-2026-07-26 `cross-owner-closeout` (CR-SW-2): thread dumps get line
    // numbers. The depositor publishes a line-less snapshot (it must stay
    // lock-free), so the reader resolves — but only fail-closed.
    // -----------------------------------------------------------------------

    use crate::runtime::stackwalker::test_support::{named_method, store_with};
    use crate::runtime::stackwalker::{LINE_NUMBER_NATIVE, LINE_NUMBER_UNKNOWN};
    use cratonvm_native_api::StackTraceEntry;
    use cratonvm_reader::attribute::LineNumberEntry;

    fn deposited_entry(
        class_id: crate::classloading::ClassId,
        method: &str,
        bci: i32,
    ) -> StackTraceEntry {
        // Exactly the shape `stackwalker::capture_frames_no_lines` deposits.
        StackTraceEntry {
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from(method),
            source_file: Some(Arc::from("Target.java")),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        }
    }

    fn registry_with_trace(trace: Vec<StackTraceEntry>) -> (ThreadRegistry, ThreadId) {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker-1", None);
        registry.set_frame_trace(tid, Arc::new(Mutex::new(trace)));
        (registry, tid)
    }

    #[test]
    fn frame_trace_of_is_line_less_and_resolved_reader_fills_it_in() {
        let (store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![
                LineNumberEntry {
                    start_pc: 0,
                    line_number: 40,
                },
                LineNumberEntry {
                    start_pc: 4,
                    line_number: 41,
                },
            ],
        )]);
        let (registry, tid) = registry_with_trace(vec![
            deposited_entry(cid, "compute", 0),
            deposited_entry(cid, "compute", 6),
        ]);

        // What every consumer sees today.
        let raw = registry.frame_trace_of(tid);
        assert_eq!(raw.len(), 2);
        assert!(raw.iter().all(|e| e.line_number == LINE_NUMBER_UNKNOWN));

        // What a reader holding a ClassStore can see instead.
        let resolved = registry.frame_trace_of_resolved(tid, &store);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].line_number, 40);
        assert_eq!(resolved[1].line_number, 41);

        // Resolution is non-destructive: the published snapshot is untouched,
        // so a second reader still starts from the deposited state.
        assert!(registry
            .frame_trace_of(tid)
            .iter()
            .all(|e| e.line_number == LINE_NUMBER_UNKNOWN));
    }

    #[test]
    fn resolved_reader_leaves_overloaded_and_native_frames_alone() {
        let (store, cid) = store_with(vec![
            named_method(
                "run",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            named_method(
                "run",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
        ]);
        let mut native = deposited_entry(cid, "run", -1);
        native.line_number = LINE_NUMBER_NATIVE;
        let (registry, tid) = registry_with_trace(vec![deposited_entry(cid, "run", 0), native]);

        let resolved = registry.frame_trace_of_resolved(tid, &store);
        assert_eq!(
            resolved[0].line_number, LINE_NUMBER_UNKNOWN,
            "a deposited frame has no method_index, so an overload set must \
             stay unknown rather than print a line from the wrong body"
        );
        assert_eq!(resolved[1].line_number, LINE_NUMBER_NATIVE);
        // Frame identity is never lost, which is the property a dump needs.
        assert_eq!(&*resolved[0].class_name, "probe/Target");
        assert_eq!(&*resolved[0].method_name, "run");
    }

    #[test]
    fn resolved_reader_on_an_unknown_thread_is_empty_not_a_panic() {
        let (store, _cid) = store_with(vec![named_method("compute", "()I", Vec::new())]);
        let registry = ThreadRegistry::new();
        assert!(registry
            .frame_trace_of_resolved(ThreadId(99), &store)
            .is_empty());
    }

    #[test]
    fn resolved_reader_after_class_unload_keeps_the_frame_and_drops_only_the_line() {
        let (mut store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            }],
        )]);
        let (registry, tid) = registry_with_trace(vec![deposited_entry(cid, "compute", 0)]);
        assert_eq!(
            registry.frame_trace_of_resolved(tid, &store)[0].line_number,
            40
        );

        // ClassIds are monotonic and never reused, so a stale id can only miss.
        let _ = store.remove(cid);
        let after = registry.frame_trace_of_resolved(tid, &store);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].line_number, LINE_NUMBER_UNKNOWN);
        assert_eq!(&*after[0].method_name, "compute");
    }

    #[test]
    fn native_blocked_thread_is_reported_as_blocked_until_woken() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "waiter", None);

        assert!(!registry.is_blocked(tid));
        registry.mark_native_thread_blocked(tid);
        assert!(registry.is_blocked(tid));
        registry.mark_native_thread_unblocked(tid);
        assert!(!registry.is_blocked(tid));

        registry.mark_dead(tid);
        assert!(!registry.is_blocked(tid));
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
        let mut pm = cratonvm_types::PointerMap::default();
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

        let mut pm = cratonvm_types::PointerMap::default();
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

        let mut pm1 = cratonvm_types::PointerMap::default();
        pm1.insert(first_addr, second_addr);
        registry.update_thread_objs_after_gc(&pm1);

        let mut pm2 = cratonvm_types::PointerMap::default();
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
    ///
    /// `n == 0` is the property. The elapsed bound only distinguishes
    /// "returned" from "blocked forever", so it is a hang backstop, not a
    /// 50 ms performance budget — a loaded runner can lose 50 ms to
    /// scheduling before the test body even starts (observed failing).
    #[test]
    fn t19_k1_wait_with_empty_registry_returns_zero() {
        let registry = ThreadRegistry::new();
        let start = std::time::Instant::now();
        let n = registry.wait_for_non_daemon_threads(None);
        assert_eq!(n, 0);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "empty wait must not block: {:?}",
            start.elapsed(),
        );
    }

    /// HelloWorld parity: a registry that contains only daemon threads
    /// waits for zero threads and returns immediately.
    ///
    /// As above: `n == 0` plus "neither daemon was joined" (only `join()`
    /// marks a thread dead) are the timing-independent properties; the
    /// elapsed bound is a hang backstop.
    #[test]
    fn t19_k1_wait_with_only_daemons_returns_zero() {
        let registry = ThreadRegistry::new();
        registry.register_with_daemon(ThreadId(1), "gc", None, true);
        registry.register_with_daemon(ThreadId(2), "finalizer", None, true);
        let start = std::time::Instant::now();
        let n = registry.wait_for_non_daemon_threads(None);
        assert_eq!(n, 0);
        assert!(registry.is_alive(ThreadId(1)));
        assert!(registry.is_alive(ThreadId(2)));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "daemon-only wait must not block: {:?}",
            start.elapsed(),
        );
    }

    /// Happy path: a non-daemon thread blocks the wait until it
    /// terminates.
    ///
    /// "Blocked until it terminated" is stated directly by the worker's
    /// completion flag: the flag is set as the worker's last action, so
    /// observing it `true` *after* the wait returned proves the ordering.
    /// The old `elapsed >= 80 ms` was a proxy for that and is not actually
    /// safe under load in the way a lower bound usually is: the clock only
    /// starts after `spawn` + `set_join_handle`, so on a loaded box the
    /// worker's 100 ms sleep can be over before `start` is even sampled
    /// (its sibling `t19_k1_wait_for_multiple_non_daemons` was observed
    /// failing that way at 134 µs).
    #[test]
    fn t19_k1_wait_blocks_until_non_daemon_finishes() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "user", None);
        let finished = Arc::new(AtomicBool::new(false));
        let f = finished.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            f.store(true, Ordering::Release);
        });
        registry.set_join_handle(tid, handle);
        let joined = registry.wait_for_non_daemon_threads(None);
        assert_eq!(joined, 1);
        assert!(
            finished.load(Ordering::Acquire),
            "the wait returned before the non-daemon thread had finished",
        );
    }

    /// A daemon thread does not extend the wait — even if it runs for a
    /// long time, the wait returns without considering it.
    ///
    /// Same flake shape as `t19_k1_mixed_only_joins_non_daemons`: the
    /// original `start.elapsed() < 50 ms` is a fixed wall-clock bound with
    /// essentially no margin, and it was observed failing under load. What
    /// must be true is that the daemon was *not waited for*, which the
    /// counters below state directly and load cannot falsify.
    #[test]
    fn t19_k1_daemon_thread_does_not_keep_main_alive() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register_with_daemon(tid, "dmn", None, true);
        let daemon_finished = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let df = daemon_finished.clone();
        let rel = release.clone();
        let handle = std::thread::spawn(move || {
            // Long-running — the wait must not block on this. The 60 s cap
            // makes a regression fail rather than hang forever.
            let cap = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !rel.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            df.store(true, Ordering::Release);
        });
        registry.set_join_handle(tid, handle);
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        assert_eq!(joined, 0, "a daemon must never be joined");
        assert!(
            !daemon_finished.load(Ordering::Acquire),
            "the wait returned only after the daemon finished — it was waited for",
        );
        // Only `join()` marks a thread dead, so this is a direct readout
        // that the daemon was skipped.
        assert!(registry.is_alive(tid));
        // Hang backstop only — well under the daemon's 60 s cap so
        // "waited for the daemon anyway" still fails here.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "daemon thread must not delay shutdown: {:?}",
            elapsed,
        );
        release.store(true, Ordering::Release);
        assert!(registry.join(tid), "daemon handle should still be present");
    }

    /// Multiple non-daemon threads — the wait blocks until ALL of them
    /// finish, and the count reflects all joined.
    ///
    /// "Waited for the slowest" is stated as a completion count taken
    /// after the wait returned, not as an elapsed-time lower bound. The
    /// old `elapsed >= 80 ms` looked like a safe-direction assertion but
    /// was not: the clock starts only after all three spawns, so on a
    /// loaded box the sleeps can already be over by then — this test was
    /// observed failing with `must wait for slowest: 134.294µs`.
    #[test]
    fn t19_k1_wait_for_multiple_non_daemons() {
        let registry = ThreadRegistry::new();
        let done = Arc::new(AtomicU64::new(0));
        for (i, sleep_ms) in [50u64, 100, 80].iter().enumerate() {
            let tid = ThreadId(i as u64 + 1);
            registry.register(tid, &format!("user-{i}"), None);
            let ms = *sleep_ms;
            let d = done.clone();
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                d.fetch_add(1, Ordering::AcqRel);
            });
            registry.set_join_handle(tid, handle);
        }
        let joined = registry.wait_for_non_daemon_threads(None);
        assert_eq!(joined, 3);
        assert_eq!(
            done.load(Ordering::Acquire),
            3,
            "the wait returned before every non-daemon thread had finished",
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
        // `joined == 1` is the property (the `Err` from `JoinHandle::join`
        // was swallowed and counted). The elapsed check only separates
        // "returned" from "deadlocked", so it is a hang backstop — the
        // former 5 s budget was observed failing on a loaded runner.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "join must complete in bounded time even when target panicked: {:?}",
            elapsed,
        );
    }

    /// Mixed daemon + non-daemon: the wait only joins the non-daemon
    /// threads. The daemon thread is left alive (its handle still in
    /// the registry — caller is expected to abandon it on exit).
    ///
    /// The property under test is WHICH threads were joined, not how long
    /// the call took. The earlier version asserted
    /// `start.elapsed() < 1s`, which is a fixed wall-clock bound over three
    /// OS-thread spawns plus a 30 ms sleep; on a loaded CI runner the
    /// spawns alone can blow it, so it flaked. Every assertion below is
    /// timing-independent except one deliberately generous hang backstop:
    ///
    /// * `joined == 1` — exactly one join succeeded.
    /// * `user_finished` — the non-daemon's body ran to completion before
    ///   the wait returned (we really did wait for it).
    /// * `!daemon_finished` — the daemon was still mid-flight when the
    ///   wait returned (we did *not* wait for it). The daemon is gated on
    ///   a flag this test owns, so this cannot be falsified by slowness:
    ///   load can only delay the observation, never open the gate.
    /// * `is_alive(dmn)` / `!is_alive(user)` — only `join()` calls
    ///   `mark_dead`, so the alive flags are a direct readout of who was
    ///   joined, independent of wall clock.
    #[test]
    fn t19_k1_mixed_only_joins_non_daemons() {
        let registry = ThreadRegistry::new();
        let user = ThreadId(1);
        let dmn = ThreadId(2);
        registry.register_with_daemon(user, "user", None, false);
        registry.register_with_daemon(dmn, "dmn", None, true);

        let user_finished = Arc::new(AtomicBool::new(false));
        let daemon_finished = Arc::new(AtomicBool::new(false));
        // Gate that keeps the daemon running until this test releases it.
        let release_daemon = Arc::new(AtomicBool::new(false));

        let uf = user_finished.clone();
        let user_h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            uf.store(true, Ordering::Release);
        });
        let df = daemon_finished.clone();
        let rd = release_daemon.clone();
        let dmn_h = std::thread::spawn(move || {
            // Long-running daemon — must NOT be joined. The 60 s cap is a
            // safety net so a regression fails instead of hanging forever.
            let cap = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !rd.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            df.store(true, Ordering::Release);
        });
        registry.set_join_handle(user, user_h);
        registry.set_join_handle(dmn, dmn_h);

        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();

        assert_eq!(joined, 1, "only the non-daemon thread may be joined");
        assert!(
            user_finished.load(Ordering::Acquire),
            "the non-daemon thread must have completed before the wait returned",
        );
        assert!(
            !daemon_finished.load(Ordering::Acquire),
            "the wait returned only after the daemon finished — it was waited for",
        );
        // Daemon is still alive (we didn't join it).
        assert!(registry.is_alive(dmn));
        // User thread is joined → marked dead.
        assert!(!registry.is_alive(user));
        // Hang backstop only. Must stay comfortably below the daemon's 60 s
        // cap so "joined the daemon anyway" still fails here rather than
        // sailing through; it is *not* a performance assertion.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "wait_for_non_daemon_threads appears to have hung: {:?}",
            elapsed,
        );

        // Release and reap the daemon so it doesn't linger for the rest of
        // the test binary's life.
        release_daemon.store(true, Ordering::Release);
        assert!(registry.join(dmn), "daemon handle should still be present");
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
        //
        // `joined == 1` *is* the assertion: had the wait not completed
        // within the deadline, the loop would have bailed out and returned
        // 0. So the deadline itself is the bound under test, and it is set
        // generously (a loaded runner needs the slack — the former 5 s
        // deadline / 2 s assertion pair was observed failing). The elapsed
        // check below is only a hang backstop.
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(60);
        let joined = registry.wait_for_non_daemon_threads(Some(deadline));
        assert_eq!(
            joined, 1,
            "the happy path must complete within a generous deadline"
        );
        assert!(
            !registry.is_alive(tid),
            "the joined thread must be marked dead"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "happy-path deadline should not extend execution: {:?}",
            start.elapsed(),
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
        let b_finished = Arc::new(AtomicBool::new(false));
        let bf = b_finished.clone();
        let reg2 = registry.clone();
        let handle_a = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            // Register a second non-daemon thread that runs for 60 ms.
            let tid_b = ThreadId(2);
            reg2.register(tid_b, "b", None);
            let h_b = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(60));
                bf.store(true, Ordering::Release);
            });
            reg2.set_join_handle(tid_b, h_b);
        });
        registry.set_join_handle(tid_a, handle_a);

        let joined = registry.wait_for_non_daemon_threads(None);
        // Both A and B must be joined. `joined == 2` plus B's completion
        // flag say "the re-snapshot caught B" directly; the former
        // `elapsed >= 80 ms` said it only by proxy and, like its siblings,
        // could be defeated by a loaded box finishing the sleeps before
        // the clock was even sampled.
        assert_eq!(joined, 2, "must catch late-spawned thread B");
        assert!(
            b_finished.load(Ordering::Acquire),
            "the wait returned before late-spawned thread B had finished",
        );
        assert!(!registry.is_alive(ThreadId(2)), "B must be joined + dead");
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

    /// Deadline-bounded wait exits before all threads are joined: an
    /// expired deadline stops the loop, leaving live non-daemon threads
    /// unjoined; the same registry then joins all of them once the
    /// deadline is lifted.
    ///
    /// The deadline is checked *between* joins — an in-flight
    /// `JoinHandle::join` is not interruptible — so the property under
    /// test is "the loop stopped handing out joins", not "the call took
    /// less than N milliseconds".
    ///
    /// The earlier version spawned five 30 ms sleepers with a 60 ms
    /// deadline and asserted only `start.elapsed() < 1s`. That measured
    /// nothing: the five sleepers run concurrently, so on an idle box all
    /// five finish at ~30 ms and the deadline never fires at all, while on
    /// a loaded CI runner five OS-thread spawns plus scheduling latency can
    /// exceed 1 s outright — a pure machine-speed assertion, and a flaky
    /// one. Here the workers are gated on a flag this test owns, so they
    /// are guaranteed still alive and unjoinable while the deadline is
    /// tested, and the outcome is decided by counters rather than a clock:
    ///
    /// * `joined == 0` with all five still `is_alive` after the expired
    ///   deadline — only `join()` calls `mark_dead`, so this is a direct
    ///   timing-independent readout that nothing was joined. Had the
    ///   deadline been ignored, the call would have blocked on the gate
    ///   (and, after the workers' own safety cap, reported 5).
    /// * `joined == 5` on the follow-up unbounded wait — proves the zero
    ///   above was the deadline's doing and not an empty snapshot.
    #[test]
    fn t19_k1_deadline_exits_before_all_joined() {
        let registry = ThreadRegistry::new();
        // Gate: the workers stay alive — and therefore unjoinable — until
        // this test releases them.
        let release = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for i in 0..5 {
            let tid = ThreadId(i as u64 + 1);
            registry.register(tid, &format!("u-{i}"), None);
            let rel = release.clone();
            let handle = std::thread::spawn(move || {
                // The 30 s cap is a safety net: a regression that ignores
                // the deadline fails loudly instead of hanging forever.
                let cap = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !rel.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            registry.set_join_handle(tid, handle);
            ids.push(tid);
        }

        // A deadline sampled *now* is already reached by the time the wait
        // checks it (`Instant` is monotonic, and the check is `>=`), so the
        // loop must bail out before its first join. No sleeping, no
        // race: this cannot be perturbed by machine load.
        let start = std::time::Instant::now();
        let deadline = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(Some(deadline));
        let elapsed = start.elapsed();

        assert_eq!(
            joined, 0,
            "an already-expired deadline must stop the wait before any join",
        );
        for tid in &ids {
            assert!(
                registry.is_alive(*tid),
                "{tid:?} was joined despite the expired deadline",
            );
        }
        // Hang backstop only — must stay well under the workers' 30 s cap
        // so a deadline regression still fails here. Not a perf assertion.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "deadline must bound the wait: {:?}",
            elapsed,
        );

        // Lift the gate: the very same registry now joins all five, which
        // is what makes the `0` above attributable to the deadline.
        release.store(true, Ordering::Release);
        assert_eq!(
            registry.wait_for_non_daemon_threads(None),
            5,
            "the unbounded wait must join every non-daemon thread",
        );
        for tid in &ids {
            assert!(!registry.is_alive(*tid), "{tid:?} should be joined + dead");
        }
    }

    // -----------------------------------------------------------------
    // ARCH-2026-07-26 — registry contention
    // -----------------------------------------------------------------

    /// Registry reads are concurrent: many threads must be able to be inside
    /// `is_alive` / `get_park_state` / `is_blocked` at the same instant.
    ///
    /// Proven by holding a *read* guard on the map for the whole window and
    /// requiring readers on other threads to complete anyway. Under the old
    /// `Mutex` this would deadlock; under an `RwLock` the readers share.
    #[test]
    fn registry_reads_do_not_exclude_each_other() {
        let registry = Arc::new(ThreadRegistry::new());
        for i in 0..8u64 {
            registry.register(ThreadId(i), &format!("t{i}"), None);
        }

        let held = registry.threads.read();

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut workers = Vec::new();
        for i in 0..8u64 {
            let r = registry.clone();
            let tx = tx.clone();
            workers.push(std::thread::spawn(move || {
                assert!(r.is_alive(ThreadId(i)));
                assert!(!r.is_blocked(ThreadId(i)));
                assert_eq!(r.java_block_state(ThreadId(i)), 0);
                assert!(r.get_park_state(ThreadId(i)).is_some());
                assert_eq!(r.thread_name(ThreadId(i)), Some(format!("t{i}")));
                assert_eq!(r.alive_count(), 8);
                let _ = tx.send(());
            }));
        }
        drop(tx);

        let mut done = 0;
        while done < 8 {
            if rx.recv_timeout(std::time::Duration::from_secs(10)).is_err() {
                break;
            }
            done += 1;
        }
        // Release before asserting/joining so a regression fails the assert
        // instead of hanging the suite.
        drop(held);
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(
            done, 8,
            "registry readers blocked each other — `threads` must be an RwLock \
             so concurrent lookups do not serialize"
        );
    }

    /// Registration still excludes readers: a write guard must block a reader,
    /// which is what keeps the map's invariants intact across insert/remove.
    #[test]
    fn registry_writes_still_exclude_readers() {
        let registry = Arc::new(ThreadRegistry::new());
        registry.register(ThreadId(1), "t", None);

        let held = registry.threads.write();
        let r = registry.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let alive = r.is_alive(ThreadId(1));
            let _ = tx.send(());
            alive
        });

        let blocked = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err();
        drop(held);
        let alive = worker.join().unwrap();
        assert!(blocked, "a writer must exclude readers");
        assert!(
            alive,
            "the reader must still get the right answer afterwards"
        );
    }

    /// The per-thread self-handle must return the *same* slot the registry
    /// holds, so a cross-thread `post_async_exception` is visible to the
    /// cached, lock-free `take_async_exception` on the target thread.
    #[test]
    fn cached_self_async_slot_is_the_registrys_slot() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "victim", None);

        // Prime the thread-local cache.
        assert!(registry.take_async_exception(tid).is_none());

        let cached = registry.self_async_slot(tid).expect("slot");
        let in_map = {
            let threads = registry.threads.read();
            Arc::clone(&threads.get(&tid).unwrap().async_exception_slot)
        };
        assert!(
            Arc::ptr_eq(&cached, &in_map),
            "the cached slot must BE the registry's slot, not a copy — \
             otherwise a posted async exception is never observed"
        );

        // Round-trip through the cache after priming.
        let mut backing = [0u64; 2];
        let throwable = dummy_aligned_objref(&mut backing);
        assert!(registry.post_async_exception(tid, throwable));
        let taken = registry.take_async_exception(tid).expect("posted");
        assert_eq!(taken.as_ptr(), throwable.as_ptr());
        assert!(registry.take_async_exception(tid).is_none());
    }

    /// The self-handle cache is keyed by a process-monotonic registry id, so a
    /// second registry that reuses the same `ThreadId` on the same OS thread
    /// never sees the first registry's slot.
    ///
    /// Keying by the registry's *address* would be unsound here: `ThreadId`s
    /// restart at 1 for every registry and a dropped registry's address can be
    /// reused.
    #[test]
    fn self_slot_cache_does_not_leak_between_registries() {
        let tid = ThreadId(1);

        let first_slot = {
            let a = ThreadRegistry::new();
            a.register(tid, "a", None);
            // Prime the cache for (a, tid).
            assert!(a.take_async_exception(tid).is_none());
            a.self_async_slot(tid).expect("slot")
        };

        let b = ThreadRegistry::new();
        b.register(tid, "b", None);
        let second_slot = b.self_async_slot(tid).expect("slot");

        assert!(
            !Arc::ptr_eq(&first_slot, &second_slot),
            "the self-slot cache served a stale registry's slot"
        );

        // And the second registry's posts really land in the second slot.
        let mut backing = [0u64; 2];
        let throwable = dummy_aligned_objref(&mut backing);
        assert!(b.post_async_exception(tid, throwable));
        assert_eq!(
            b.take_async_exception(tid).map(|o| o.as_ptr()),
            Some(throwable.as_ptr())
        );
    }

    /// The census reports EVERY thread parked in `Object.wait()`, not the one
    /// the watchdog happened to reach first.
    ///
    /// This is the property
    /// `known-issues/netty/parameterizedsslhandlertest-residual-stalls` was
    /// blocked on: with one waiter reported, "nothing completed this promise"
    /// and "the thread that would have completed it is itself parked" are
    /// indistinguishable. A thread that has LEFT the wait must not appear, or
    /// the census would manufacture the second reading.
    #[test]
    fn waiting_monitor_census_lists_every_waiter_and_only_waiters() {
        let registry = ThreadRegistry::new();
        let mut b1 = [0u64; 2];
        let mut b2 = [0u64; 2];
        let mut b3 = [0u64; 2];
        let (m1, m2, m3) = (
            dummy_aligned_objref(&mut b1),
            dummy_aligned_objref(&mut b2),
            dummy_aligned_objref(&mut b3),
        );
        for (i, name) in ["main", "reactor-1", "reactor-2"].iter().enumerate() {
            registry.register(ThreadId(i as u64), name, None);
        }
        assert!(
            registry.waiting_monitor_census().is_empty(),
            "nobody is waiting yet"
        );

        registry.set_jmx_waiting_monitor(ThreadId(0), m1);
        registry.set_jmx_waiting_monitor(ThreadId(2), m3);
        let rows = registry.waiting_monitor_census();
        assert_eq!(rows.len(), 2, "both waiters, and only them: {rows:?}");
        assert_eq!(rows[0].0, 0);
        assert_eq!(rows[0].1, "main");
        assert!(std::ptr::eq(rows[0].3.as_ptr(), m1.as_ptr()));
        assert_eq!(rows[1].0, 2);
        assert!(std::ptr::eq(rows[1].3.as_ptr(), m3.as_ptr()));

        // A thread that leaves the wait drops out; one that enters appears.
        registry.take_jmx_waiting_monitor(ThreadId(0));
        registry.set_jmx_waiting_monitor(ThreadId(1), m2);
        let rows = registry.waiting_monitor_census();
        assert_eq!(
            rows.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![1, 2],
            "the census must track the slot, not a high-water mark"
        );
    }

    /// A reaped (terminated, purged) entry must not make a cached slot unsafe:
    /// the `Arc` keeps it alive, and the thread is dead so nobody can post to
    /// it any more.
    #[test]
    fn cached_slot_survives_entry_reaping() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "victim", None);
        assert!(registry.take_async_exception(tid).is_none()); // prime

        registry.mark_dead(tid);
        // Force the terminated-entry purge by exceeding the retention cap.
        for i in 0..(TERMINATED_THREAD_ENTRY_CAP as u64 + 8) {
            let t = ThreadId(i + 2);
            registry.register(t, "filler", None);
            registry.mark_dead(t);
        }

        // Still answerable, still `None`, and crucially still sound.
        assert!(registry.take_async_exception(tid).is_none());
        assert!(!registry.is_alive(tid));
    }
}
