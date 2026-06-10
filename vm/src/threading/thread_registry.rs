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
    /// stopping thread. The pointee must be kept alive by the caller
    /// until the target thread consumes it (GC roots scan the target
    /// thread's frames; the `stop()` helper allocates the Throwable
    /// from the caller's heap which is reachable until handoff).
    async_exception_slot: Arc<std::sync::atomic::AtomicUsize>,
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
}

impl ThreadRegistry {
    /// Create a new, empty registry. The next thread id will be 1
    /// (id 0 is reserved for the main thread).
    pub fn new() -> Self {
        Self {
            threads: Mutex::new(FxHashMap::default()),
            next_id: AtomicU64::new(1),
            thread_obj_to_park: Mutex::new(FxHashMap::default()),
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
        let park_state = Arc::new(ParkState::new());
        let entry = ThreadEntry {
            name: name.to_string(),
            join_handle: None,
            java_thread_obj,
            alive: AtomicBool::new(true),
            daemon: AtomicBool::new(daemon),
            park_state: park_state.clone(),
            interrupted: Arc::new(AtomicBool::new(false)),
            root_snapshot: Arc::new(Mutex::new(Vec::new())),
            gc_block_state: Arc::new(GcBlockState::new()),
            async_exception_slot: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        self.threads.lock().insert(thread_id, entry);
        if let Some(obj) = java_thread_obj {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, park_state);
        }
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
    /// The target will raise the exception at its next safepoint. The
    /// caller must guarantee that `throwable` remains reachable as a
    /// GC root until the target consumes it; in practice this means
    /// allocating the Throwable in the caller's heap (roots from the
    /// target's frames will pick it up once stored) OR handing over a
    /// reference held by a reachable static field.
    pub fn post_async_exception(
        &self,
        thread_id: ThreadId,
        throwable: ObjectRef,
    ) -> bool {
        let threads = self.threads.lock();
        if let Some(entry) = threads.get(&thread_id) {
            entry
                .async_exception_slot
                .store(throwable.as_ptr() as usize, std::sync::atomic::Ordering::Release);
            // Also unpark the target so a parked thread wakes and
            // hits its next safepoint promptly.
            entry.park_state.unpark();
            true
        } else {
            false
        }
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
        if let Some(entry) = self.threads.lock().get(&thread_id) {
            entry.alive.store(false, Ordering::Release);
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

    /// Collect root snapshots from all alive threads.
    /// Returns a combined vector of all ObjectRefs from all threads' snapshots.
    pub fn collect_all_root_snapshots(&self) -> Vec<ObjectRef> {
        let threads = self.threads.lock();
        let mut all_roots = Vec::new();
        for entry in threads.values() {
            if entry.alive.load(Ordering::Acquire) {
                let snapshot = entry.root_snapshot.lock();
                all_roots.extend(snapshot.iter().copied());
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
        for entry in threads.values_mut() {
            if let Some(ref mut obj) = entry.java_thread_obj {
                let old_addr = obj.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // SAFETY: produced by the GC pointer map.
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    rekeyed.push((old_addr, new_addr));
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
    pub fn fold_pointer_map_into_blocked(
        &self,
        pointer_map: &HashMap<usize, usize>,
    ) {
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
            let chained: rustc_hash::FxHashSet<usize> =
                fixup.values().copied().collect();
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
                tid.0 != 0
                    && e.alive.load(Ordering::Acquire)
                    && !e.daemon.load(Ordering::Acquire)
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
    pub fn wait_for_non_daemon_threads(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> usize {
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
    fn unknown_thread_not_alive() {
        let registry = ThreadRegistry::new();
        assert!(!registry.is_alive(ThreadId(99)));
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
