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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::event::{AwtEvent, AwtEventData, PeerId, event_id};

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
    /// invocation event; drained and signalled by `poll_event` /
    /// `wait_event` / `drain_events` after the matching event is dequeued.
    ///
    /// NOTE: this signals on **dequeue**, not after `Runnable.run()`
    /// returns.  See `invoke_and_wait` for the semantic gap and TODO.
    pending_invocations: Mutex<HashMap<u64, CompletionHandle>>,
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
        }
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
    pub fn invoke_later(&self, callback_id: u64, peer_id: PeerId) {
        let event = AwtEvent::invocation(peer_id, Self::now(), callback_id);
        self.post_event(event);
    }

    /// Post an `InvocationEvent` and block until the EDT has dispatched it.
    ///
    /// # Panics
    ///
    /// Panics if called from the EDT (would deadlock).
    ///
    /// # Semantics & TODO
    ///
    /// Real `EventQueue.invokeAndWait` returns *after* the `Runnable.run()`
    /// method has finished executing on the EDT.  This implementation only
    /// guarantees that the wait returns after the corresponding
    /// `AwtEventData::Invocation` event has been **dequeued** by
    /// `poll_event` / `wait_event` / `drain_events`.
    ///
    /// In the current codebase the JVM-side native method
    /// `java/awt/EventQueue.getNextEvent` (see `natives.rs`) just calls
    /// `poll_event` and discards the result without actually invoking the
    /// Java `Runnable`, so "dequeued" is the strongest signal available
    /// without modifying that dispatch site.  Once the dispatch site is
    /// taught to look the callback up and run it, that site should call
    /// [`Self::signal_invocation_complete`] **after** `Runnable.run()`
    /// returns to provide true `invokeAndWait` semantics.
    ///
    /// If the EDT is stopped before the event is dispatched, all pending
    /// handles are released via `stop()` so this call returns instead of
    /// hanging forever.
    pub fn invoke_and_wait(&self, callback_id: u64, peer_id: PeerId) {
        assert!(
            !is_edt(),
            "invoke_and_wait must not be called on the EDT"
        );

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
    }

    /// Signal that the invocation registered under `callback_id` has
    /// completed.  Called automatically by `poll_event` / `wait_event` /
    /// `drain_events` after the matching invocation event is dequeued.
    ///
    /// Returns `true` if a waiter was registered and notified.  Safe to
    /// call with any callback_id -- unknown ids are silently ignored, so
    /// `invoke_later` (which never registers a handle) costs only one
    /// HashMap lookup.
    fn signal_invocation_complete(&self, callback_id: u64) -> bool {
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

    /// If `event` is an invocation event with a registered completion
    /// handle, notify the waiting `invoke_and_wait` caller.  No-op for
    /// every other event kind.
    fn notify_if_invocation(&self, event: &AwtEvent) {
        if let AwtEventData::Invocation { callback_id } = &event.data {
            self.signal_invocation_complete(*callback_id);
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
        edt.invoke_and_wait(99, PeerId(1));
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

        edt.invoke_and_wait(7777, PeerId(2));
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
}
