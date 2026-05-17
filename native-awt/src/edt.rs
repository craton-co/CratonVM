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
use rustjvm_types::ObjectRef;

use crate::event::{AwtEvent, AwtEventData, PeerId, event_id};

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

static EDT: OnceLock<EventDispatchThread> = OnceLock::new();

/// Returns the global EDT singleton, creating it on first access.
pub fn get_edt() -> &'static EventDispatchThread {
    EDT.get_or_init(EventDispatchThread::new)
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
    /// dispatch()V` native via [`Self::take_runnable`].
    ///
    /// If a posted `InvocationEvent` is never dispatched (e.g. because the
    /// app GC's it before draining the queue, or the EDT shuts down with
    /// pending events) the entry would leak forever. We cap the map at
    /// [`Self::MAX_RUNNABLES`] entries with FIFO eviction so misbehaving
    /// callers can't grow this map without bound. The `insertion_order`
    /// VecDeque tracks the eviction order.
    runnables: Mutex<FxHashMap<u64, ObjectRef>>,
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
    pub fn register_runnable(&self, callback_id: u64, runnable: ObjectRef) {
        let mut map = self.runnables.lock();
        let mut order = self.runnables_order.lock();
        // Evict oldest entries if at capacity.  Both locks held: the
        // dispatcher in `take_runnable` is blocked behind `map` here, so
        // we know `map.len()` and `order` cannot diverge mid-eviction.
        while map.len() >= Self::MAX_RUNNABLES {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            } else {
                break;
            }
        }
        if map.insert(callback_id, runnable).is_none() {
            order.push_back(callback_id);
        }
    }

    /// Remove and return the Runnable registered for `callback_id`, if
    /// any.  Called by `InvocationEvent.dispatch()V`.
    ///
    /// RACE FIX (round-5 audit): hold both `runnables` and
    /// `runnables_order` for the full remove+order-cleanup so that a
    /// concurrent `register_runnable` cannot pop the head of `order`
    /// while we have already removed the corresponding `map` entry but
    /// not yet swept the order tracker.  See `register_runnable` for the
    /// full hazard description.
    pub fn take_runnable(&self, callback_id: u64) -> Option<ObjectRef> {
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

    // -- lifecycle ----------------------------------------------------------

    /// Mark the EDT as running.
    pub fn start(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Mark the EDT as stopped and wake any blocked waiters so they can
    /// observe the shutdown.
    pub fn stop(&self) {
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
        self.runnables.lock().clear();
        self.runnables_order.lock().clear();
    }

    /// Returns `true` if the EDT is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
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
    pub fn invoke_later_runnable(&self, runnable: ObjectRef, peer_id: PeerId) -> u64 {
        let id = self.next_callback_id();
        self.register_runnable(id, runnable);
        self.invoke_later(id, peer_id);
        id
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
        let handle: CompletionHandle =
            Arc::new((Mutex::new(false), Condvar::new()));
        {
            let mut map = self.pending_invocations.lock();
            map.insert(callback_id, Arc::clone(&handle));
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
    pub fn invoke_and_wait_runnable(
        &self,
        runnable: ObjectRef,
        peer_id: PeerId,
    ) -> Result<u64, InvokeAndWaitError> {
        if is_edt() {
            return Err(InvokeAndWaitError::OnEdt);
        }
        let id = self.next_callback_id();
        self.register_runnable(id, runnable);
        self.invoke_and_wait(id, peer_id)?;
        Ok(id)
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
            let x1 = (*dx).min(*sx);
            let y1 = (*dy).min(*sy);
            let x2 = (*dx + *dw).max(*sx + *sw);
            let y2 = (*dy + *dh).max(*sy + *sh);
            *dx = x1;
            *dy = y1;
            *dw = x2 - x1;
            *dh = y2 - y1;
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
mod tests {
    use super::*;
    use crate::event::{event_id, AwtEvent, PeerId};

    fn make_edt() -> EventDispatchThread {
        EventDispatchThread::new()
    }

    #[test]
    fn post_and_poll() {
        let edt = make_edt();
        assert!(edt.poll_event().is_none());

        let evt = AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 100);
        edt.post_event(evt);
        assert_eq!(edt.queue_length(), 1);

        let got = edt.poll_event().unwrap();
        assert_eq!(got.id, event_id::WINDOW_OPENED);
        assert_eq!(got.source_peer_id, PeerId(1));
        assert!(edt.poll_event().is_none());
    }

    #[test]
    fn drain_events() {
        let edt = make_edt();
        edt.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
        edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(1), 1));
        edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSED, PeerId(1), 2));

        let all = edt.drain_events();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, event_id::WINDOW_OPENED);
        assert_eq!(all[1].id, event_id::WINDOW_CLOSING);
        assert_eq!(all[2].id, event_id::WINDOW_CLOSED);
        assert_eq!(edt.queue_length(), 0);
    }

    #[test]
    fn invoke_later_posts_invocation_event() {
        let edt = make_edt();
        edt.invoke_later(42, PeerId(7));

        let evt = edt.poll_event().unwrap();
        assert_eq!(evt.id, event_id::INVOCATION_DEFAULT);
        assert_eq!(evt.source_peer_id, PeerId(7));
        if let AwtEventData::Invocation { callback_id } = &evt.data {
            assert_eq!(*callback_id, 42);
        } else {
            panic!("expected Invocation");
        }
    }

    #[test]
    fn start_stop_lifecycle() {
        let edt = make_edt();
        assert!(!edt.is_running());
        edt.start();
        assert!(edt.is_running());
        edt.stop();
        assert!(!edt.is_running());
    }

    #[test]
    fn wait_event_returns_none_on_timeout() {
        let edt = make_edt();
        let result = edt.wait_event(10); // 10ms timeout
        assert!(result.is_none());
    }

    #[test]
    fn wait_event_returns_existing() {
        let edt = make_edt();
        edt.post_event(AwtEvent::window(event_id::WINDOW_ACTIVATED, PeerId(1), 0));
        let evt = edt.wait_event(100).unwrap();
        assert_eq!(evt.id, event_id::WINDOW_ACTIVATED);
    }

    #[test]
    fn coalesce_paint_events_same_peer() {
        let edt = make_edt();
        let peer = PeerId(10);

        // Post two paint events for the same peer with overlapping rects.
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 50, 50));
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 1, 25, 25, 50, 50));

        edt.coalesce_paint_events();

        assert_eq!(edt.queue_length(), 1);
        let evt = edt.poll_event().unwrap();
        if let AwtEventData::Paint {
            x,
            y,
            width,
            height,
        } = &evt.data
        {
            // Bounding union of (0,0,50,50) and (25,25,50,50) = (0,0,75,75)
            assert_eq!((*x, *y, *width, *height), (0, 0, 75, 75));
        } else {
            panic!("expected Paint");
        }
    }

    #[test]
    fn coalesce_does_not_merge_different_peers() {
        let edt = make_edt();
        edt.post_event(AwtEvent::paint(event_id::PAINT, PeerId(1), 0, 0, 0, 50, 50));
        edt.post_event(AwtEvent::paint(event_id::PAINT, PeerId(2), 1, 0, 0, 50, 50));

        edt.coalesce_paint_events();
        assert_eq!(edt.queue_length(), 2);
    }

    #[test]
    fn coalesce_preserves_non_paint_events() {
        let edt = make_edt();
        let peer = PeerId(1);
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 10, 10));
        edt.post_event(AwtEvent::mouse(
            event_id::MOUSE_CLICKED,
            peer,
            1,
            5,
            5,
            1,
            1,
            0,
        ));
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 2, 20, 20, 10, 10));

        edt.coalesce_paint_events();

        // The mouse event separates the two paints, so they should NOT merge.
        assert_eq!(edt.queue_length(), 3);
        let events = edt.drain_events();
        assert_eq!(events[0].id, event_id::PAINT);
        assert_eq!(events[1].id, event_id::MOUSE_CLICKED);
        assert_eq!(events[2].id, event_id::PAINT);
    }

    #[test]
    fn edt_thread_local_marker() {
        // By default we are not on the EDT.
        assert!(!is_edt());
        assert!(!EventDispatchThread::is_dispatch_thread());

        // Mark this thread.
        mark_as_edt();
        assert!(is_edt());
        assert!(EventDispatchThread::is_dispatch_thread());

        // Clean up for other tests that may run on the same thread.
        IS_EDT.set(false);
    }

    #[test]
    fn invoke_and_wait_from_edt_returns_error_instead_of_panicking() {
        // Round-9 misc fix regression test: calling invokeAndWait from
        // the EDT must NOT panic (which previously unwound across the
        // JNI boundary). It must surface InvokeAndWaitError::OnEdt so
        // the native bridge can throw IllegalStateException on the
        // Java thread instead.
        let edt = make_edt();
        mark_as_edt();
        let result = edt.invoke_and_wait(1, PeerId(0));
        // Clean up for other tests on this thread before asserting.
        IS_EDT.set(false);
        assert_eq!(result, Err(InvokeAndWaitError::OnEdt));
        assert_eq!(
            InvokeAndWaitError::OnEdt.jdk_message(),
            "Cannot call invokeAndWait from the event dispatcher thread"
        );
    }

    #[test]
    fn invoke_and_wait_from_non_edt() {
        let edt = Arc::new(make_edt());
        edt.start();

        // Spawn a consumer that drains the queue via `poll_event` so the
        // invocation-completion signal fires.
        let edt2 = Arc::clone(&edt);
        let consumer = std::thread::spawn(move || {
            while edt2.is_running() {
                if edt2.poll_event().is_some() {
                    break; // consumed the invocation event
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        // This should complete once the consumer dequeues the event and
        // poll_event signals the completion handle.
        edt.invoke_and_wait(99, PeerId(1)).expect("non-EDT caller must succeed");
        consumer.join().unwrap();
        edt.stop();
    }

    #[test]
    fn invoke_and_wait_releases_on_stop() {
        // If the EDT is stopped before the event is dispatched,
        // `invoke_and_wait` must return instead of hanging.
        let edt = Arc::new(make_edt());
        edt.start();

        let edt2 = Arc::clone(&edt);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            edt2.stop();
        });

        edt.invoke_and_wait(7777, PeerId(2)).expect("must release once EDT stops");
        stopper.join().unwrap();
    }

    #[test]
    fn post_event_coalesces_wakes() {
        // Sanity check: two posts only emit one mpsc wake when the queue
        // was already non-empty for the second push.
        let edt = make_edt();
        edt.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
        edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(1), 1));

        // Drain the wake channel; only the first post should have sent.
        let recv = edt.wake_receiver.lock();
        assert!(recv.try_recv().is_ok());
        assert!(recv.try_recv().is_err());
    }

    #[test]
    fn wait_event_wakes_on_post() {
        let edt = Arc::new(make_edt());
        let edt2 = Arc::clone(&edt);

        let handle = std::thread::spawn(move || {
            // Post an event after a short delay.
            std::thread::sleep(Duration::from_millis(20));
            edt2.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
        });

        // Should wake up within the timeout thanks to the posted event.
        let evt = edt.wait_event(2000);
        assert!(evt.is_some());
        assert_eq!(evt.unwrap().id, event_id::WINDOW_OPENED);
        handle.join().unwrap();
    }

    /// Build a dummy `ObjectRef` for tests.  Uses an 8-aligned non-null
    /// fake pointer — never dereferenced.
    fn fake_object_ref(seed: u64) -> ObjectRef {
        let ptr = ((seed + 1) << 3) as *mut u8; // guaranteed 8-aligned & non-null
        unsafe { ObjectRef::from_raw(ptr) }
    }

    #[test]
    fn runnable_registry_register_and_take() {
        let edt = make_edt();
        let runnable = fake_object_ref(42);
        let id = edt.next_callback_id();
        edt.register_runnable(id, runnable);
        assert_eq!(edt.take_runnable(id), Some(runnable));
        assert_eq!(edt.take_runnable(id), None, "second take should be empty");
    }

    #[test]
    fn next_callback_id_is_monotonic_and_nonzero() {
        let edt = make_edt();
        let a = edt.next_callback_id();
        let b = edt.next_callback_id();
        assert!(a > 0 && b > a, "ids must be monotonic, got {a} then {b}");
    }

    #[test]
    fn notify_if_invocation_skips_when_runnable_pending() {
        // When a Runnable is registered for the callback id, dequeue must
        // NOT signal completion -- the dispatch native owns the signal.
        let edt = make_edt();
        let runnable = fake_object_ref(7);
        let id = edt.next_callback_id();
        edt.register_runnable(id, runnable);

        // Register a completion handle to detect spurious signalling.
        let handle: CompletionHandle = Arc::new((Mutex::new(false), Condvar::new()));
        edt.pending_invocations.lock().insert(id, Arc::clone(&handle));

        edt.post_event(AwtEvent::invocation(PeerId(0), 0, id));
        let _ = edt.poll_event();

        // The Runnable is still registered, so dequeue must have been
        // silent: the completion handle stays un-flipped.
        assert!(!*handle.0.lock(), "dequeue must not signal while runnable is pending");
    }

    #[test]
    fn signal_invocation_complete_after_dispatch_unblocks_waiter() {
        // Simulate the natives-driven flow: register, post, dequeue (no
        // signal because runnable still pending), then explicitly take +
        // signal as the dispatch native would.
        let edt = Arc::new(make_edt());
        edt.start();
        let runnable = fake_object_ref(123);
        let id = edt.next_callback_id();
        edt.register_runnable(id, runnable);

        let edt2 = Arc::clone(&edt);
        let dispatcher = std::thread::spawn(move || {
            // Wait for the event to land then "dispatch" it.
            loop {
                if let Some(_evt) = edt2.poll_event() {
                    assert_eq!(edt2.take_runnable(id), Some(runnable));
                    // pretend Runnable.run() ran here
                    let signalled = edt2.signal_invocation_complete(id);
                    assert!(signalled, "waiter should have been registered");
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        edt.invoke_and_wait(id, PeerId(0)).expect("non-EDT caller must succeed");
        dispatcher.join().unwrap();
        edt.stop();
    }

    #[test]
    fn coalesce_three_adjacent_paints() {
        let edt = make_edt();
        let peer = PeerId(1);
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 10, 10));
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 1, 10, 10, 10, 10));
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 2, 20, 20, 10, 10));

        edt.coalesce_paint_events();
        assert_eq!(edt.queue_length(), 1);

        let evt = edt.poll_event().unwrap();
        if let AwtEventData::Paint { x, y, width, height } = &evt.data {
            assert_eq!((*x, *y, *width, *height), (0, 0, 30, 30));
        } else {
            panic!("expected Paint");
        }
    }

    #[test]
    fn coalesce_and_drain_combines_paints_and_drains_in_one_lock() {
        // Round-8 EDT throughput fix: the combined entry point must
        // (a) coalesce adjacent same-peer paints exactly like
        // `coalesce_paint_events` would, and (b) return every remaining
        // event in queue order — leaving the queue empty.
        let edt = make_edt();
        let peer = PeerId(7);
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 10, 10));
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 1, 5, 5, 10, 10));
        edt.post_event(AwtEvent::mouse(
            event_id::MOUSE_CLICKED, peer, 2, 1, 1, 1, 1, 0,
        ));
        edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 3, 50, 50, 10, 10));

        let drained = edt.coalesce_and_drain();
        // Two paints merged + one mouse + one trailing paint = 3 events.
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].id, event_id::PAINT);
        if let AwtEventData::Paint { x, y, width, height } = &drained[0].data {
            // Bounding union of (0,0,10,10) and (5,5,10,10) = (0,0,15,15)
            assert_eq!((*x, *y, *width, *height), (0, 0, 15, 15));
        } else {
            panic!("expected Paint");
        }
        assert_eq!(drained[1].id, event_id::MOUSE_CLICKED);
        assert_eq!(drained[2].id, event_id::PAINT);
        assert_eq!(edt.queue_length(), 0);
    }
}
