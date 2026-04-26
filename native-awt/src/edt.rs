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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

use crate::event::{AwtEvent, AwtEventData, PeerId, event_id};

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
    }

    /// Returns `true` if the EDT is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    // -- posting events -----------------------------------------------------

    /// Append an event to the tail of the queue and wake the EDT.
    pub fn post_event(&self, event: AwtEvent) {
        self.queue.lock().push_back(event);
        let _ = self.wake_sender.lock().send(());
    }

    /// Post an `InvocationEvent` (the equivalent of `EventQueue.invokeLater`).
    pub fn invoke_later(&self, callback_id: u64, peer_id: PeerId) {
        let event = AwtEvent::invocation(peer_id, Self::now(), callback_id);
        self.post_event(event);
    }

    /// Post an `InvocationEvent` and block until it has been consumed by the
    /// dispatch loop.
    ///
    /// # Panics
    ///
    /// Panics if called from the EDT (would deadlock).
    pub fn invoke_and_wait(&self, callback_id: u64, peer_id: PeerId) {
        assert!(
            !is_edt(),
            "invoke_and_wait must not be called on the EDT"
        );

        let done = Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()));
        let done2 = Arc::clone(&done);

        // We wrap the real callback_id with a sentinel that the caller can
        // watch.  The dispatch loop itself does not know about this --
        // instead we spin here polling until the event has been dequeued.
        let event = AwtEvent::invocation(peer_id, Self::now(), callback_id);
        self.post_event(event);

        // Spin-wait until the event we just posted is no longer in the queue.
        // This is correct because `poll_event` / `wait_event` remove the
        // event, and the caller (Java side) will process it synchronously on
        // the EDT before the next event is dequeued.
        loop {
            {
                let q = self.queue.lock();
                let still_queued = q.iter().any(|e| {
                    if let AwtEventData::Invocation { callback_id: cid } = &e.data {
                        *cid == callback_id
                    } else {
                        false
                    }
                });
                if !still_queued {
                    break;
                }
            }
            // Avoid busy-waiting -- yield briefly.
            std::thread::sleep(Duration::from_micros(100));
            // Also break if the EDT stopped.
            if !self.is_running() {
                break;
            }
        }
        drop(done2);
        drop(done);
    }

    // -- consuming events ---------------------------------------------------

    /// Non-blocking dequeue.  Returns `None` if the queue is empty.
    pub fn poll_event(&self) -> Option<AwtEvent> {
        self.queue.lock().pop_front()
    }

    /// Blocking dequeue with a timeout (in milliseconds).  Returns `None` if
    /// the timeout expires without an event arriving.
    pub fn wait_event(&self, timeout_ms: u64) -> Option<AwtEvent> {
        // Fast path: check the queue first.
        {
            let mut q = self.queue.lock();
            if let Some(evt) = q.pop_front() {
                return Some(evt);
            }
        }

        // Slow path: wait for a wake signal.
        let recv = self.wake_receiver.lock();
        let _ = recv.recv_timeout(Duration::from_millis(timeout_ms));
        drop(recv);

        // Check queue again after waking.
        self.queue.lock().pop_front()
    }

    /// Drain all pending events in one shot.
    pub fn drain_events(&self) -> Vec<AwtEvent> {
        self.queue.lock().drain(..).collect()
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
        let edt = make_edt();
        edt.start();

        // Spawn a consumer that drains the queue.
        let q = Arc::clone(&edt.queue);
        let running = Arc::clone(&edt.running);
        let consumer = std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let evt = { q.lock().pop_front() };
                if evt.is_some() {
                    break; // consumed the invocation event
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        // This should complete once the consumer drains the event.
        edt.invoke_and_wait(99, PeerId(1));
        consumer.join().unwrap();
        edt.stop();
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
