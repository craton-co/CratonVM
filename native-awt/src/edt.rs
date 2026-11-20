// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Event Dispatch Thread (EDT) coordinator.
//!
//! In Java, all GUI operations run on the EDT. This module provides:
//!
//! 1. A thread-safe event queue (`VecDeque<AwtEvent>`)
//! 2. `invoke_later` / `invoke_and_wait` to post work to the queue
//! 3. Blocking and non-blocking dequeue for the dispatch loop
//! 4. Paint-event coalescing to reduce redundant repaints
//!
//! The EDT does **not** spawn an OS thread itself -- the JVM manages the
//! thread and calls into this module to pump events.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

use crate::event::{event_id, AwtEvent, AwtEventData, PeerId};

// ---------------------------------------------------------------------------
// Runnable registry — used by invokeLater / invokeAndWait
// ---------------------------------------------------------------------------
//
// `EventQueue.invokeLater(Runnable)` posts an `InvocationEvent` to the EDT
// whose `dispatch()` method must eventually call `Runnable.run()` on the EDT.
//
// Before round-2 of N2-5 the runnable was thrown away — the EDT just polled
// the queue, signalled `invokeAndWait` waiters on dequeue, and returned
// nothing to the Java-side `EventQueue.getNextEvent`.  Every `invokeLater`
// callback silently disappeared.
//
// The fix:
//   1. `invoke_later` / `invoke_and_wait` register the Runnable here keyed
//      by a freshly-allocated callback id.
//   2. The Java-side `EventQueue.getNextEvent` native (see `natives.rs`)
//      allocates a `java/awt/event/InvocationEvent`, binds its identity
//      hash to the callback id, and returns it.
//   3. The EDT calls `InvocationEvent.dispatch()V` (also a native, in
//      `natives.rs`), which looks up the Runnable, invokes
//      `run()V` virtually, signals `invoke_and_wait` waiters, and drops
//      the registry entries.

// ---------------------------------------------------------------------------
// Completion handles for `invoke_and_wait`
// ---------------------------------------------------------------------------

/// Shared (mutex, condvar) pair used to signal completion of an
/// `invoke_and_wait` invocation.  The bool is `true` once the EDT has
/// (at minimum) dequeued the corresponding invocation event.
type CompletionHandle = Arc<(Mutex<bool>, Condvar)>;

/// A global-root handle together with the VM whose global-ref table it
/// indexes.
///
/// gc-common w22-a
/// (`common-w21a-more-process-wide-global-root-handle-caches`, item 2): the
/// `usize` that `NativeContext::add_global_root` returns is an index into the
/// CALLING VM's own table. The EDT singleton (and the natives' peer-source
/// table) are process-wide, so a handle stored there without its VM was
/// resolved -- or, on a size-cap eviction, released -- against whichever
/// VM's `ctx` happened to make the call: an unrelated object of that VM ran
/// as a `Runnable`, or an unrelated root of that VM was freed. Every row now
/// carries its `vm`; only that VM may resolve or release `handle`
/// (`natives.rs::release_vm_roots` parks another VM's handle for its owner).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmRoot {
    /// `NativeContext::vm_identity()` of the VM that created the root.
    pub vm: usize,
    /// Opaque handle returned by that VM's `NativeContext::add_global_root`.
    pub handle: usize,
}

/// A pending Runnable registered for an `invokeLater` / `invokeAndWait`
/// invocation.
///
/// The entry stores a persistent VM global-root handle, not a bare
/// `ObjectRef`. The native dispatch bridge resolves the handle through the
/// active `NativeContext` immediately before calling `Runnable.run()`, then
/// releases the global root after dispatch.
#[derive(Clone, Copy)]
struct RunnableEntry {
    /// The Runnable's global root, tagged with its VM.
    root: VmRoot,
}

// ---------------------------------------------------------------------------
// Thread-local EDT marker
// ---------------------------------------------------------------------------

thread_local! {
    static IS_EDT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the current thread as the EDT.  Called once when the JVM starts the
/// dispatch thread.
pub fn mark_as_edt() {
    IS_EDT.set(true);
}

/// Returns `true` if the current thread has been marked as the EDT.
pub fn is_edt() -> bool {
    IS_EDT.get()
}

// ---------------------------------------------------------------------------
// Global EDT singleton
// ---------------------------------------------------------------------------

/// The queue of the "no VM" identity `0` (the mock contexts' default
/// `vm_identity()`, and the pure-Rust callers of [`get_edt`]).
static EDT: OnceLock<Arc<EventDispatchThread>> = OnceLock::new();

/// Returns the process-wide default EDT (VM identity `0`), creating it on
/// first access.
///
/// gc-common w23-a: the natives no longer use this for a real VM -- every
/// VM has its own queue ([`edt_for_vm`]). It stays the queue of identity `0`
/// (mock contexts) and of pure-Rust callers (`vm/tests/t7_desktop_conformance`).
pub fn get_edt() -> &'static EventDispatchThread {
    default_edt()
}

fn default_edt() -> &'static Arc<EventDispatchThread> {
    EDT.get_or_init(|| Arc::new(EventDispatchThread::new()))
}

/// Per-VM event queues, keyed by `NativeContext::vm_identity()`.
///
/// gc-common w23-a (`common-w22a-awt-state-is-process-wide-and-keyed-by-identity-hash`,
/// item 1): there used to be ONE queue for the process, so VM B's
/// `EventQueue.getNextEvent` dequeued VM A's mouse / key / window / paint /
/// invocation events (w22-a made a foreign invocation skip rather than run in
/// the wrong heap, which silently dropped VM A's `invokeLater` work). Each VM
/// now gets its own [`EventDispatchThread`] on first use; its teardown
/// (`natives::forget_vm_awt_roots` -> [`forget_vm_edt`]) removes and closes
/// it. Not compatibility state: the rows are per VM by key.
fn vm_edts() -> &'static parking_lot::RwLock<FxHashMap<usize, Arc<EventDispatchThread>>> {
    static INSTANCE: OnceLock<parking_lot::RwLock<FxHashMap<usize, Arc<EventDispatchThread>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| parking_lot::RwLock::new(FxHashMap::default()))
}

/// The event queue of VM `vm`, created on first use. Identity `0` answers the
/// [`get_edt`] queue. The common path is one shared read lock and an `Arc`
/// clone.
pub fn edt_for_vm(vm: usize) -> Arc<EventDispatchThread> {
    if vm == 0 {
        return Arc::clone(default_edt());
    }
    if let Some(edt) = vm_edts().read().get(&vm) {
        return Arc::clone(edt);
    }
    let mut map = vm_edts().write();
    Arc::clone(
        map.entry(vm)
            .or_insert_with(|| Arc::new(EventDispatchThread::new())),
    )
}

/// Remove VM `vm`'s event queue (VM teardown) and hand it back, so the caller
/// can [`EventDispatchThread::close`] it. `None` when the VM never used AWT.
/// Identity `0`'s queue ([`get_edt`]) is never removed.
pub fn forget_vm_edt(vm: usize) -> Option<Arc<EventDispatchThread>> {
    if vm == 0 {
        return None;
    }
    vm_edts().write().remove(&vm)
}

/// Whether VM `vm` currently has a queue of its own (tests, diagnostics).
pub fn vm_has_edt(vm: usize) -> bool {
    vm != 0 && vm_edts().read().contains_key(&vm)
}

// ---------------------------------------------------------------------------
// invokeAndWait errors
// ---------------------------------------------------------------------------

/// Error type returned by [`EventDispatchThread::invoke_and_wait`] and
/// [`EventDispatchThread::invoke_and_wait_runnable`].
///
/// Prior to round-9 misc fix, calling `invokeAndWait` from the EDT
/// triggered an `assert!`, which panicked across the JNI boundary — UB
/// on most VMs and observable as a process abort. The native bridge in
/// `natives.rs` now converts this into a Java-visible
/// `IllegalStateException` whose message matches the JDK:
/// `"Cannot call invokeAndWait from the event dispatcher thread"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeAndWaitError {
    /// The caller is already running on the EDT — would deadlock.
    OnEdt,
}

impl InvokeAndWaitError {
    /// JDK-spec'd error message. Kept as a single accessor so the
    /// native bridge and any tests stay in sync on wording.
    pub fn jdk_message(&self) -> &'static str {
        match self {
            InvokeAndWaitError::OnEdt => {
                "Cannot call invokeAndWait from the event dispatcher thread"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EventDispatchThread
// ---------------------------------------------------------------------------

/// The Event Dispatch Thread coordinator.
pub struct EventDispatchThread {
    queue: Arc<Mutex<VecDeque<AwtEvent>>>,
    running: Arc<AtomicBool>,
    /// Set once by [`Self::close`] (the owning VM's teardown, gc-common
    /// w23-a) and never cleared: a blocked [`Self::wait_event_blocking`]
    /// returns `None` and a later `invoke_and_wait` returns at once. Never set
    /// on a live VM's queue, so it changes nothing while the VM runs.
    closed: AtomicBool,
    wake_sender: Mutex<mpsc::Sender<()>>,
    wake_receiver: Arc<Mutex<mpsc::Receiver<()>>>,
    /// Side-table of pending `invoke_and_wait` completions keyed by
    /// callback_id.  Populated by `invoke_and_wait` before posting the
    /// invocation event; drained and signalled by the `InvocationEvent.
    /// dispatch()V` native AFTER `Runnable.run()` returns (see
    /// [`Self::signal_invocation_complete`]).
    pending_invocations: Mutex<HashMap<u64, CompletionHandle>>,
    /// Side-table of pending Runnables, keyed by callback_id.  Populated by
    /// `register_runnable` (called from the `invokeLater` /
    /// `invokeAndWait` natives); read & removed by the `InvocationEvent.
    /// dispatch()V` native via [`Self::take_runnable_root`].
    ///
    /// Each entry holds the Runnable as a JNI global root of the VM that
    /// registered it ([`VmRoot`]): the root keeps the object alive and the
    /// collector remaps it, so a moving collection between `invokeLater`
    /// and dispatch is harmless. (It used to hold a bare `ObjectRef` plus a
    /// GC-count stamp and fail closed across any collection; some comments
    /// below still describe that scheme.) Only the registering VM may
    /// resolve or release the root; a VM's rows are dropped at its teardown
    /// ([`Self::forget_vm_runnables`]).
    ///
    /// If a posted `InvocationEvent` is never dispatched (e.g. because the
    /// app GC's it before draining the queue, or the EDT shuts down with
    /// pending events) the entry would leak forever. We cap the map at
    /// [`Self::MAX_RUNNABLES`] entries with FIFO eviction so misbehaving
    /// callers can't grow this map without bound. The `insertion_order`
    /// VecDeque tracks the eviction order.
    runnables: Mutex<FxHashMap<u64, RunnableEntry>>,
    runnables_order: Mutex<VecDeque<u64>>,
    /// Monotonic counter for callback ids.  We can't reuse the Runnable's
    /// identity hash because (a) two separate `invokeLater(sameRunnable)`
    /// calls must each dispatch once, and (b) identity hash codes are i32
    /// and could collide.
    next_callback_id: AtomicU64,
}

impl EventDispatchThread {
    /// Create a new EDT coordinator.  Does **not** start an OS thread.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicBool::new(false)),
            closed: AtomicBool::new(false),
            wake_sender: Mutex::new(tx),
            wake_receiver: Arc::new(Mutex::new(rx)),
            pending_invocations: Mutex::new(HashMap::new()),
            runnables: Mutex::new(FxHashMap::default()),
            runnables_order: Mutex::new(VecDeque::new()),
            // Start above 0 so callers can safely use `0` as "no id".
            next_callback_id: AtomicU64::new(1),
        }
    }

    /// Allocate a fresh monotonic callback id.
    pub fn next_callback_id(&self) -> u64 {
        self.next_callback_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Maximum number of pending Runnables held in the side-table before
    /// FIFO eviction kicks in. See `runnables` field doc.
    const MAX_RUNNABLES: usize = 10_000;

    /// Register a Runnable ObjectRef under a callback id.  Called from the
    /// `invokeLater` / `invokeAndWait` natives BEFORE posting the
    /// corresponding `AwtEvent::invocation` so that the dispatch site
    /// can always find the Runnable.
    ///
    /// RACE FIX (round-5 audit): `runnables` and `runnables_order` MUST be
    /// updated atomically.  Before this fix the two locks were taken
    /// separately in both `register_runnable` and `take_runnable`, so an
    /// eviction in `register_runnable` could `pop_front` from `order` an
    /// id whose `runnables` entry the dispatcher had just removed (line
    /// 171, between the two lock-acquisitions in `take_runnable`).  The
    /// eviction's `map.remove(&old)` then silently no-op'd while still
    /// having consumed `old` from the order deque — leaving subsequent
    /// dispatchers unable to detect that the slot they thought was theirs
    /// had been re-allocated to a different runnable.
    ///
    /// We now take both locks atomically (consistent ordering: runnables
    /// before runnables_order, the same as `take_runnable` and `stop`) so
    /// the eviction loop sees an internally-consistent view of the table.
    /// `gc_gen` is the GC collection count captured by the caller at
    /// registration time (read from an active `NativeContext` via
    /// `gc_collection_count()`). It is stored alongside the Runnable and
    /// re-checked at dispatch by [`Self::take_runnable_root`] so a stale
    /// (post-collection) pointer is never resurrected — the same fail-closed
    /// gate the peer-source side-table uses (`lookup_peer_source`).
    ///
    /// Returns the roots this call displaced (evicted or replaced). They may
    /// belong to ANY VM: the caller releases each through its own VM
    /// (`natives.rs::release_vm_roots`), never through its `ctx` blindly.
    pub fn register_runnable(&self, callback_id: u64, root: VmRoot) -> Vec<VmRoot> {
        let mut map = self.runnables.lock();
        let mut order = self.runnables_order.lock();
        let mut removed_roots = Vec::new();
        // Evict oldest entries if at capacity.  Both locks held: the
        // dispatcher in `take_runnable_root` is blocked behind `map`
        // here, so we know `map.len()` and `order` cannot diverge
        // mid-eviction.
        while map.len() >= Self::MAX_RUNNABLES {
            if let Some(old) = order.pop_front() {
                if let Some(entry) = map.remove(&old) {
                    removed_roots.push(entry.root);
                }
            } else {
                break;
            }
        }
        if let Some(previous) = map.insert(callback_id, RunnableEntry { root }) {
            removed_roots.push(previous.root);
        } else {
            order.push_back(callback_id);
        }
        removed_roots
    }

    /// Remove and return the [`RunnableEntry`] registered for `callback_id`,
    /// if any.  This is the raw, GC-unchecked removal; callers on the
    /// dispatch path MUST go through [`Self::take_runnable_root`] so the
    /// GC-generation gate is applied.
    ///
    /// RACE FIX (round-5 audit): hold both `runnables` and
    /// `runnables_order` for the full remove+order-cleanup so that a
    /// concurrent `register_runnable` cannot pop the head of `order`
    /// while we have already removed the corresponding `map` entry but
    /// not yet swept the order tracker.  See `register_runnable` for the
    /// full hazard description.
    fn take_runnable_entry(&self, callback_id: u64) -> Option<RunnableEntry> {
        let mut map = self.runnables.lock();
        let mut order = self.runnables_order.lock();
        let removed = map.remove(&callback_id);
        if removed.is_some() {
            // Remove from order tracker. Linear scan is fine: the order
            // deque is bounded by `MAX_RUNNABLES`, and successful dispatches
            // typically take the head (cheap).
            if let Some(pos) = order.iter().position(|&id| id == callback_id) {
                order.remove(pos);
            }
        }
        removed
    }

    /// Remove and return the Runnable registered for `callback_id`, but ONLY
    /// when the live GC count (`current_gc_gen`) still matches the count
    /// stamped at registration.  Called by `InvocationEvent.dispatch()V`.
    ///
    /// Returns `None` (fail closed) when the entry is absent OR a (moving)
    /// collection has run since registration. In the latter case the moving
    /// collector may have relocated or freed the Runnable object, so
    /// dereferencing the cached pointer would be a use-after-free / type
    /// confusion. We therefore drop the entry and let dispatch skip the
    /// stale Runnable rather than calling `run()` through a dangling
    /// pointer. This mirrors `natives.rs::lookup_peer_source`, which fails
    /// closed across the same GC boundary for cached peer-source pointers.
    ///
    /// The entry is unconditionally removed (whether or not the generation
    /// matched): a stale Runnable is never going to become safe to dispatch,
    /// so keeping it would only leak the side-table slot.
    ///
    /// The returned root may belong to another VM (the queue is
    /// process-wide): the caller resolves it only when `root.vm` is its own
    /// VM and otherwise hands it to `natives.rs::release_vm_roots`.
    pub fn take_runnable_root(&self, callback_id: u64) -> Option<VmRoot> {
        let entry = self.take_runnable_entry(callback_id)?;
        if entry.root.handle == 0 {
            // No root was ever taken for this Runnable -- refuse to
            // resurrect anything. The caller skips dispatch in this case.
            return None;
        }
        Some(entry.root)
    }

    /// Drop every Runnable row `vm` registered, releasing nothing: the VM is
    /// being torn down and its global-ref table dies with it
    /// (`natives.rs::forget_vm_awt_roots`). Any `invokeAndWait` waiter still
    /// registered for one of those rows is signalled, so its completion
    /// handle does not outlive the VM. Returns the dropped callback ids.
    pub fn forget_vm_runnables(&self, vm: usize) -> Vec<u64> {
        let dropped: Vec<u64> = {
            let mut map = self.runnables.lock();
            let mut order = self.runnables_order.lock();
            let dropped: Vec<u64> = map
                .iter()
                .filter(|(_, entry)| entry.root.vm == vm)
                .map(|(&id, _)| id)
                .collect();
            if !dropped.is_empty() {
                for id in &dropped {
                    map.remove(id);
                }
                order.retain(|id| map.contains_key(id));
            }
            dropped
        };
        for &id in &dropped {
            self.signal_invocation_complete(id);
        }
        dropped
    }

    // -- lifecycle ----------------------------------------------------------

    /// Mark the EDT as running.
    pub fn start(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Mark the EDT as stopped and wake any blocked waiters so they can
    /// observe the shutdown.
    ///
    /// Returns the roots of the Runnables that never ran, each tagged with its
    /// VM (release each through that VM).
    pub fn stop(&self) -> Vec<VmRoot> {
        self.running.store(false, Ordering::SeqCst);
        // Wake anyone blocked in `wait_event`.
        let _ = self.wake_sender.lock().send(());
        // Release every thread blocked in `invoke_and_wait` -- their event
        // will never be dispatched now, so flip the flag to true and notify
        // so the caller returns instead of hanging.  Callers should re-check
        // `is_running()` if they need to distinguish completion from
        // shutdown.
        let pending: Vec<CompletionHandle> = {
            let mut map = self.pending_invocations.lock();
            map.drain().map(|(_, h)| h).collect()
        };
        for handle in pending {
            let (lock, cv) = &*handle;
            let mut done = lock.lock();
            *done = true;
            cv.notify_all();
        }
        // Drop any Runnables that never made it to dispatch.  Without
        // this, an EDT restart would re-dispatch stale Runnables when
        // their callback ids happen to be reused (we use a monotonic
        // counter, so collisions are unlikely — but the leak is real).
        let removed_roots = {
            let mut map = self.runnables.lock();
            let mut order = self.runnables_order.lock();
            order.clear();
            map.drain().map(|(_, entry)| entry.root).collect::<Vec<_>>()
        };
        removed_roots
    }

    /// Returns `true` if the EDT is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Close the queue for good (gc-common w23-a: the owning VM is being torn
    /// down). Drops every queued event, wakes a dispatch loop blocked in
    /// [`Self::wait_event_blocking`] (it returns `None`), releases every
    /// `invoke_and_wait` waiter, and returns the roots of the Runnables that
    /// never ran -- the caller drops them unreleased, since the VM's
    /// global-ref table dies with it. Idempotent.
    ///
    /// `closed` is published BEFORE [`Self::stop`] drains the waiters, and
    /// `invoke_and_wait` re-checks it after filing its handle: a waiter that
    /// files after the drain sees `closed` and returns instead of hanging.
    pub fn close(&self) -> Vec<VmRoot> {
        self.closed.store(true, Ordering::SeqCst);
        self.queue.lock().clear();
        self.stop()
    }

    /// Whether [`Self::close`] has run.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    // -- posting events -----------------------------------------------------

    /// Append an event to the tail of the queue and wake the EDT.
    ///
    /// The mpsc wake is **coalesced**: it is only sent when the queue was
    /// empty before the push.  If the queue already had pending events the
    /// receiver is either dispatching them right now or will pick the new
    /// one up on its next loop iteration, so an extra wake is wasted work
    /// (one syscall per posted event under high-throughput input streams
    /// like mouse-move/paint).
    pub fn post_event(&self, event: AwtEvent) {
        let was_empty = {
            let mut q = self.queue.lock();
            let was_empty = q.is_empty();
            q.push_back(event);
            was_empty
        };
        if was_empty {
            let _ = self.wake_sender.lock().send(());
        }
    }

    /// Post an `InvocationEvent` (the equivalent of `EventQueue.invokeLater`).
    ///
    /// Low-level form: the caller is responsible for inserting the
    /// matching Runnable into the registry via [`Self::register_runnable`]
    /// BEFORE calling this (otherwise the dispatch native will silently
    /// no-op).  Use [`Self::invoke_later_runnable`] if you have an
    /// `ObjectRef` in hand.
    pub fn invoke_later(&self, callback_id: u64, peer_id: PeerId) {
        let event = AwtEvent::invocation(peer_id, Self::now(), callback_id);
        self.post_event(event);
    }

    /// High-level form: allocate a callback id, register the Runnable,
    /// post the invocation event.  Returns the callback id for callers
    /// that want to correlate with completion later.
    ///
    /// `gc_gen` is the GC collection count captured by the caller (via
    /// `gc_collection_count()`) and stamped on the registry entry so the
    /// dispatch site can fail closed if a moving collection runs before the
    /// Runnable is dispatched. See [`Self::register_runnable`].
    pub fn invoke_later_runnable(&self, root: VmRoot, peer_id: PeerId) -> (u64, Vec<VmRoot>) {
        let id = self.next_callback_id();
        let removed_roots = self.register_runnable(id, root);
        self.invoke_later(id, peer_id);
        (id, removed_roots)
    }

    /// Post an `InvocationEvent` and block until the EDT has dispatched it.
    ///
    /// # Errors
    ///
    /// Returns `Err(InvokeAndWaitError::OnEdt)` if called from the EDT
    /// itself — calling `invokeAndWait` from the dispatch thread would
    /// deadlock, so the JDK rejects it with an `Error`. The native-side
    /// callers turn this into an `IllegalStateException` on the Java
    /// thread (see `natives.rs::invokeAndWait`) rather than panicking
    /// across the JNI boundary.
    ///
    /// # Semantics
    ///
    /// Returns *after* the matching `InvocationEvent.dispatch()V` native
    /// has finished running the registered `Runnable` and called
    /// [`Self::signal_invocation_complete`].  This matches the real
    /// `EventQueue.invokeAndWait` contract.
    ///
    /// If the EDT is stopped before the event is dispatched, all pending
    /// handles are released via `stop()` so this call returns instead of
    /// hanging forever.
    ///
    /// As with [`Self::invoke_later`], the caller is responsible for
    /// registering the Runnable via [`Self::register_runnable`] before
    /// invoking this — use [`Self::invoke_and_wait_runnable`] for the
    /// runnable-first convenience form.
    pub fn invoke_and_wait(
        &self,
        callback_id: u64,
        peer_id: PeerId,
    ) -> Result<(), InvokeAndWaitError> {
        if is_edt() {
            // Don't panic — propagate so the native layer can raise the
            // JDK-spec'd error on the Java thread instead of unwinding
            // across the JNI boundary (which is UB on most VMs).
            return Err(InvokeAndWaitError::OnEdt);
        }

        // Register a completion handle BEFORE posting the event, otherwise
        // an extremely fast EDT could dispatch and try to signal an entry
        // that does not yet exist.
        let handle: CompletionHandle = Arc::new((Mutex::new(false), Condvar::new()));
        {
            let mut map = self.pending_invocations.lock();
            map.insert(callback_id, Arc::clone(&handle));
        }
        // gc-common w23-a: a closed queue (its VM is being torn down) will
        // never dispatch this, and `close` may already have drained the
        // waiters: return rather than wait forever. Never true on a live VM.
        if self.is_closed() {
            self.pending_invocations.lock().remove(&callback_id);
            return Ok(());
        }

        let event = AwtEvent::invocation(peer_id, Self::now(), callback_id);
        self.post_event(event);

        // Block on the condvar until either the event is dispatched
        // (`signal_invocation_complete` flips the bool) or the EDT shuts
        // down (`stop` flips the bool for every pending entry).
        let (lock, cv) = &*handle;
        let mut done = lock.lock();
        while !*done {
            cv.wait(&mut done);
        }
        Ok(())
    }

    /// High-level form of [`Self::invoke_and_wait`]: registers `runnable`
    /// under a fresh callback id, posts the invocation event, blocks
    /// until the `dispatch()V` native finishes running it.
    ///
    /// Returns the allocated callback id on success; surfaces
    /// [`InvokeAndWaitError::OnEdt`] when called from the EDT so the
    /// native bridge can throw `IllegalStateException` instead of
    /// panicking across the JNI boundary.
    ///
    /// `gc_gen` is the GC collection count captured by the caller (via
    /// `gc_collection_count()`) and stamped on the registry entry so the
    /// dispatch site can fail closed if a moving collection runs before the
    /// Runnable is dispatched. See [`Self::register_runnable`].
    pub fn invoke_and_wait_runnable(
        &self,
        root: VmRoot,
        peer_id: PeerId,
    ) -> Result<(u64, Vec<VmRoot>), InvokeAndWaitError> {
        if is_edt() {
            return Err(InvokeAndWaitError::OnEdt);
        }
        let id = self.next_callback_id();
        let removed_roots = self.register_runnable(id, root);
        self.invoke_and_wait(id, peer_id)?;
        Ok((id, removed_roots))
    }

    /// Signal that the invocation registered under `callback_id` has
    /// completed.  Called by the `InvocationEvent.dispatch()V` native
    /// AFTER the registered `Runnable.run()` returns, so blocked
    /// `invoke_and_wait` callers observe true "run finished" semantics.
    ///
    /// Returns `true` if a waiter was registered and notified.  Safe to
    /// call with any callback_id -- unknown ids are silently ignored, so
    /// `invoke_later` (which never registers a handle) costs only one
    /// HashMap lookup.
    pub fn signal_invocation_complete(&self, callback_id: u64) -> bool {
        let handle = {
            let mut map = self.pending_invocations.lock();
            map.remove(&callback_id)
        };
        if let Some(handle) = handle {
            let (lock, cv) = &*handle;
            let mut done = lock.lock();
            *done = true;
            cv.notify_all();
            true
        } else {
            false
        }
    }

    /// If `event` is an invocation event AND no Runnable has been
    /// registered for it (i.e. there's no dispatch site that will call
    /// [`Self::signal_invocation_complete`] explicitly), notify the
    /// waiting `invoke_and_wait` caller on dequeue so it doesn't hang
    /// forever.  This is the legacy / pure-Rust path; when the natives
    /// layer drives dispatch through `InvocationEvent.dispatch()V`, that
    /// native owns the signal and this method's check finds the
    /// Runnable still registered and stays silent.
    fn notify_if_invocation(&self, event: &AwtEvent) {
        if let AwtEventData::Invocation { callback_id } = &event.data {
            let has_runnable = self.runnables.lock().contains_key(callback_id);
            if !has_runnable {
                self.signal_invocation_complete(*callback_id);
            }
        }
    }

    // -- consuming events ---------------------------------------------------

    /// Non-blocking dequeue.  Returns `None` if the queue is empty.
    ///
    /// If the dequeued event is an invocation, any `invoke_and_wait` caller
    /// blocked on it is released.  TODO: ideally this signal would fire
    /// after `Runnable.run()` returns, not at dequeue time -- see
    /// [`Self::invoke_and_wait`].
    pub fn poll_event(&self) -> Option<AwtEvent> {
        let evt = self.queue.lock().pop_front();
        if let Some(ref e) = evt {
            self.notify_if_invocation(e);
        }
        evt
    }

    /// Peek at the next event without dequeueing it.  Returns `None` if the
    /// queue is empty.
    ///
    /// Unlike [`Self::poll_event`] this does NOT fire the
    /// `invoke_and_wait` completion signal — peeking must be observation-only
    /// so a Java `EventQueue.peekEvent()` call cannot accidentally release a
    /// blocked `invokeAndWait` waiter before the event is actually dispatched.
    pub fn peek_event(&self) -> Option<AwtEvent> {
        self.queue.lock().front().cloned()
    }

    /// Block the calling thread until an event is available, the timeout
    /// expires, or the supplied `interrupted` predicate returns `true`.
    ///
    /// This is the bridge used by `EventQueue.getNextEvent`: the JDK
    /// contract is to block indefinitely on an empty queue, returning only
    /// when an event arrives or the calling thread is interrupted. The
    /// caller passes `interrupted` so the Java-side interrupt flag (held
    /// by the VM, not this crate) gets consulted between wake attempts.
    ///
    /// `max_wait_ms` is the *total* upper bound on how long we will
    /// block. `chunk_ms` (clamped to ≥1) is the per-iteration wake
    /// interval used to re-check the `interrupted` predicate while
    /// asleep. `max_wait_ms == u64::MAX` means "block indefinitely" —
    /// appropriate for the EDT dispatch loop, which only exits when the
    /// calling thread is interrupted.
    ///
    /// Note: unlike `wait_event` this method intentionally does NOT
    /// consult `is_running()`. A VM's queue (`edt::edt_for_vm`) is
    /// never explicitly `start()`-ed in production — the field is only
    /// flipped on by unit tests — so honouring `is_running()` here would
    /// cause the production `getNextEvent` native to return null
    /// immediately and the JDK dispatch loop to peg at 100% CPU.
    ///
    /// Returns `Some(evt)` on success, `None` on interrupt / timeout.
    pub fn wait_event_blocking<F: Fn() -> bool>(
        &self,
        max_wait_ms: u64,
        chunk_ms: u64,
        interrupted: F,
    ) -> Option<AwtEvent> {
        let chunk_ms = chunk_ms.max(1);
        let start = std::time::Instant::now();
        let deadline = if max_wait_ms == u64::MAX {
            None
        } else {
            Some(start + Duration::from_millis(max_wait_ms))
        };
        loop {
            // gc-common w23-a: the owning VM's teardown closed this queue.
            if self.is_closed() {
                return None;
            }
            // Fast path first: an event is already on the queue.
            {
                let mut q = self.queue.lock();
                if let Some(evt) = q.pop_front() {
                    drop(q);
                    self.notify_if_invocation(&evt);
                    return Some(evt);
                }
            }
            // Honour shutdown / interrupt between sleeps so a long-blocked
            // EDT dispatch loop unwinds promptly.
            if interrupted() {
                return None;
            }
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return None;
                }
            }
            let recv = self.wake_receiver.lock();
            let _ = recv.recv_timeout(Duration::from_millis(chunk_ms));
            drop(recv);
        }
    }

    /// Blocking dequeue with a timeout (in milliseconds).  Returns `None` if
    /// the timeout expires without an event arriving.
    pub fn wait_event(&self, timeout_ms: u64) -> Option<AwtEvent> {
        // Fast path: check the queue first.
        {
            let mut q = self.queue.lock();
            if let Some(evt) = q.pop_front() {
                drop(q);
                self.notify_if_invocation(&evt);
                return Some(evt);
            }
        }

        // Slow path: wait for a wake signal.
        let recv = self.wake_receiver.lock();
        let _ = recv.recv_timeout(Duration::from_millis(timeout_ms));
        drop(recv);

        // Check queue again after waking.
        let evt = self.queue.lock().pop_front();
        if let Some(ref e) = evt {
            self.notify_if_invocation(e);
        }
        evt
    }

    /// Drain all pending events in one shot.
    pub fn drain_events(&self) -> Vec<AwtEvent> {
        let events: Vec<AwtEvent> = self.queue.lock().drain(..).collect();
        for e in &events {
            self.notify_if_invocation(e);
        }
        events
    }

    /// Round-8 EDT throughput fix: coalesce adjacent same-peer paint
    /// events in the queue AND drain the result in a single lock
    /// acquisition.
    ///
    /// Background: the prior two-step pattern
    /// `coalesce_paint_events(); drain_events();` took the queue mutex
    /// twice and let an event-poster slip in between, so a paint event
    /// landing right after `coalesce_paint_events` returned was not
    /// coalesced with whatever the EDT was about to process. Under
    /// contention (mouse-drag generating paint storms) that meant
    /// queue-tail merges were lost and the EDT did unnecessary repaint
    /// passes.
    ///
    /// This single-lock variant performs coalescing in-place on the
    /// queue then drains, so a concurrent post during the operation
    /// either lands before the lock (and gets coalesced) or after
    /// the drain returns (and is processed on the next EDT cycle) —
    /// never half-and-half.
    ///
    /// **TODO(round-10, double-buffered EDT)**: after coalescing, the
    /// coalesced paint events should render into the per-Frame back-
    /// buffer (already allocated on the peer as `image_id` — see
    /// [`crate::peer::ComponentPeer::image_id`]) and then submit a
    /// single `blit_buffer` to the platform window. The current code
    /// path lets Java-side paint() draw directly through Graphics2D
    /// natives onto whatever the peer's `image_id` points to, which is
    /// already the back-buffer for components that opt into it; the
    /// front/back swap (a `PlatformBackend::blit_buffer` call) happens
    /// when Java calls `Toolkit.sync()` or when the OS issues an
    /// `expose`/`WM_PAINT`. That is the JDK's
    /// "draw-then-present" model and matches the current behaviour.
    ///
    /// True double-buffering with an atomic swap inside this method
    /// would require either (a) the platform backend exposing a swap-
    /// chain primitive (Win32: separate compatible-DC blit; X11:
    /// XdbeSwapBuffers; Cocoa: CALayer contents swap), or (b) holding
    /// the back-buffer here and `blit_buffer`-ing it after every paint
    /// batch. (a) is invasive across all three backends; (b) duplicates
    /// the peer's `image_id` buffer in Rust just to issue the blit.
    /// Both are out of scope for the round-10 EDT-only pass — flagged
    /// here so the platform-backend refactor that adds the swap-chain
    /// primitive knows to wire it through this drain point.
    pub fn coalesce_and_drain(&self) -> Vec<AwtEvent> {
        let mut q = self.queue.lock();

        if q.len() >= 2 {
            let mut coalesced: VecDeque<AwtEvent> = VecDeque::with_capacity(q.len());
            while let Some(evt) = q.pop_front() {
                if !evt.is_paint() {
                    coalesced.push_back(evt);
                    continue;
                }
                let merged = if let Some(last) = coalesced.back_mut() {
                    if last.is_paint() && last.source_peer_id == evt.source_peer_id {
                        Self::merge_paint_rects(last, &evt);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !merged {
                    coalesced.push_back(evt);
                }
            }
            *q = coalesced;
        }

        let events: Vec<AwtEvent> = q.drain(..).collect();
        // Drop the queue lock before invoking `notify_if_invocation`,
        // which acquires `runnables` and may itself take other locks —
        // keeping the queue lock held across that path would re-introduce
        // the contention this method exists to eliminate.
        drop(q);
        for e in &events {
            self.notify_if_invocation(e);
        }
        events
    }

    /// Number of events currently in the queue.
    pub fn queue_length(&self) -> usize {
        self.queue.lock().len()
    }

    // -- coalescing ---------------------------------------------------------

    /// Merge adjacent `PAINT`/`UPDATE` events that target the same component
    /// into a single event whose rectangle is the bounding union.
    ///
    /// This reduces redundant repaints when the queue backs up.
    pub fn coalesce_paint_events(&self) {
        let mut q = self.queue.lock();
        if q.len() < 2 {
            return;
        }

        let mut coalesced: VecDeque<AwtEvent> = VecDeque::with_capacity(q.len());

        while let Some(evt) = q.pop_front() {
            if !evt.is_paint() {
                coalesced.push_back(evt);
                continue;
            }

            // Try to merge with the last coalesced event if it is a paint for
            // the same peer.
            let merged = if let Some(last) = coalesced.back_mut() {
                if last.is_paint() && last.source_peer_id == evt.source_peer_id {
                    Self::merge_paint_rects(last, &evt);
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if !merged {
                coalesced.push_back(evt);
            }
        }

        *q = coalesced;
    }

    /// Returns `true` if the calling thread is the EDT.
    pub fn is_dispatch_thread() -> bool {
        is_edt()
    }

    // -- helpers -------------------------------------------------------------

    fn merge_paint_rects(dest: &mut AwtEvent, src: &AwtEvent) {
        if let (
            AwtEventData::Paint {
                x: dx,
                y: dy,
                width: dw,
                height: dh,
            },
            AwtEventData::Paint {
                x: sx,
                y: sy,
                width: sw,
                height: sh,
            },
        ) = (&mut dest.data, &src.data)
        {
            let x1 = (*dx as i64).min(*sx as i64);
            let y1 = (*dy as i64).min(*sy as i64);
            let x2 = (*dx as i64 + (*dw).max(0) as i64).max(*sx as i64 + (*sw).max(0) as i64);
            let y2 = (*dy as i64 + (*dh).max(0) as i64).max(*sy as i64 + (*sh).max(0) as i64);
            *dx = x1.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            *dy = y1.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            *dw = (x2 - x1).clamp(0, i32::MAX as i64) as i32;
            *dh = (y2 - y1).clamp(0, i32::MAX as i64) as i32;
        }
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl Default for EventDispatchThread {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
