use super::*;

impl FinalizerThread {
    pub fn new() -> Self {
        Self {
            finalization_queue: Mutex::new(FinalizerQueue::default()),
            running: AtomicBool::new(false),
            delivery: Arc::new(DeliverySignal::new()),
            delivery_start_claimed: AtomicBool::new(false),
            delivery_thread: AtomicU64::new(0),
            gc_notifications_handed_off: AtomicBool::new(false),
            extra_runs_requeued: AtomicU64::new(0),
        }
    }

    /// Count one accepted re-queue for an owed extra `finalize()` (see
    /// [`Self::extra_runs_requeued`]).
    pub fn note_extra_run_requeued(&self) {
        self.extra_runs_requeued.fetch_add(1, Ordering::Relaxed);
    }

    /// Accepted re-queues for an owed extra `finalize()` (tests, diagnostics).
    pub fn extra_runs_requeued(&self) -> u64 {
        self.extra_runs_requeued.load(Ordering::Relaxed)
    }

    /// Record that a door handed this VM's queued GC notifications to the
    /// delivery thread (gc-common w7-d). Call BEFORE requesting the batch: the
    /// batch reads the mark after `DeliverySignal::begin_batch`, which takes
    /// the signal's lock, so a batch opened for that request always sees it.
    pub fn note_gc_notifications_handed_off(&self) {
        self.gc_notifications_handed_off
            .store(true, Ordering::Release);
    }

    /// Delivery-thread side: take (and lower) the mark
    /// [`Self::note_gc_notifications_handed_off`] raised. `true` when a door
    /// handed notifications over since the last take.
    pub fn take_gc_notifications_handed_off(&self) -> bool {
        self.gc_notifications_handed_off
            .swap(false, Ordering::AcqRel)
    }

    /// The delivery thread's wake-up channel (gc-common w3-d).
    pub fn delivery_signal(&self) -> Arc<DeliverySignal> {
        Arc::clone(&self.delivery)
    }

    /// Ask the delivery thread for a batch. Always recorded — a thread that
    /// is still starting serves it with its first batch — and answers whether
    /// a delivery thread is already serving.
    pub fn request_delivery(&self) -> bool {
        self.delivery.request();
        self.delivery_thread.load(Ordering::Acquire) != 0
    }

    /// `true` for exactly one caller per VM: the one that must spawn the
    /// delivery thread. A caller whose spawn then fails hands the claim back
    /// with [`Self::release_delivery_thread_start`].
    pub fn claim_delivery_thread_start(&self) -> bool {
        self.delivery_start_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Undo a [`Self::claim_delivery_thread_start`] whose spawn failed.
    pub fn release_delivery_thread_start(&self) {
        self.delivery_start_claimed.store(false, Ordering::Release);
    }

    /// Whether a delivery thread has ever been claimed for this VM (it may
    /// still be starting).
    pub fn delivery_thread_claimed(&self) -> bool {
        self.delivery_start_claimed.load(Ordering::Acquire)
    }

    /// Published by the delivery thread once it can serve: its VM thread id.
    pub fn set_delivery_thread(&self, thread_id: u64) {
        self.delivery_thread
            .store(thread_id.saturating_add(1), Ordering::Release);
    }

    /// The serving delivery thread's VM thread id, if any.
    pub fn delivery_thread(&self) -> Option<u64> {
        match self.delivery_thread.load(Ordering::Acquire) {
            0 => None,
            n => Some(n - 1),
        }
    }

    /// gci i42/rp (`gci-i42rp-a-delivery-thread-that-unwinds-is-never-replaced-FIXED-20261016`):
    /// the delivery thread `thread_id` is gone -- its OS thread unwound out of
    /// its loop (a Rust panic under a finalizer, a cleaner action or a GC
    /// listener). Stop naming it as the serving thread and hand the start
    /// claim back, so the next request starts a fresh thread (or, with none
    /// startable, the doors drain inline) instead of handing every later batch
    /// to a thread that will never serve it: before this the claim stayed
    /// taken and `delivery_thread()` kept answering the dead id, so every
    /// forced drain waited out the finalizer timeout and, under the full
    /// hand-off, nothing was ever delivered again.
    ///
    /// A thread that unwinds before it published itself (`delivery_thread()`
    /// still `None`) holds the claim too and hands it back the same way.
    /// `false`, and nothing changed, when a different thread is the serving
    /// one. Call once per thread (the VM's unwind guard does).
    pub fn retire_delivery_thread(&self, thread_id: u64) -> bool {
        let me = thread_id.saturating_add(1);
        match self
            .delivery_thread
            .compare_exchange(me, 0, Ordering::AcqRel, Ordering::Acquire)
        {
            // Published and serving: unpublished now.
            Ok(_) => {}
            // Never published: the claim is still this thread's.
            Err(0) => {}
            Err(_) => return false,
        }
        self.delivery_start_claimed.store(false, Ordering::Release);
        true
    }

    /// Enqueue an object the [`ReferenceProcessor`] has just reported from a
    /// finalizer row that was NEVER enqueued before — `to_finalize`, the
    /// processor's own finalization queue, or the resurrection channel's
    /// `dead_finalizers` (which the processor's `finalizer_referent_addresses`
    /// fed). Returns `false` only for a duplicate already waiting in the queue.
    ///
    /// # Never dropped at capacity (gc-common w4-d, 2026-09-23)
    ///
    /// Unlike the legacy `enqueue` door (removed, gcj j4/rf), this never
    /// refused past `FINALIZER_QUEUE_MAX_CAPACITY`. Every caller has already flagged (or is
    /// about to flag) the object's processor row `enqueued`
    /// (`mark_finalizer_enqueued`, `to_finalize`), so a refusal here was not a
    /// deferral: `finalizer_referent_addresses` never offers the row again, the
    /// object — rooted only by this queue — is freed at the next collection, and
    /// its `finalize()` silently never runs. HotSpot's finalizer queue has no
    /// cap: a program producing finalizable garbage faster than the finalizer
    /// drains it keeps those objects alive and ends in `OutOfMemoryError`, not
    /// in skipped finalizers. The queue entries themselves are 8 bytes plus an
    /// index slot; the objects they root are what costs memory, as there. With
    /// the reference-delivery thread draining asynchronously (w3-d / w4-d) the
    /// backlog can now legitimately exceed the old cap, so the drop went from
    /// theoretical to reachable. Crossing the old cap is still logged, once
    /// per crossing.
    ///
    /// # Why the legacy `enqueue` was wrong for these (gc-common w1-d, 2026-09-23)
    ///
    /// `enqueue` refused any address in `already_finalized`, a set keyed by the
    /// ADDRESS a finalized object had when it was dequeued. Nothing in
    /// production ever pruned it (`cleanup_collected` had no caller) or
    /// relocated it, so it grew by one entry per object ever finalized, and —
    /// the defect — once a finalized object died, a NEW finalizable object
    /// allocated at the same address had its `finalize()` refused, silently and
    /// forever: the processor flags its row `enqueued` as it reports it, so
    /// nothing ever offers it again. Address reuse is the normal case under a
    /// free-list sweep and under bump allocation into a recycled G1 region.
    ///
    /// The refusal was never needed on this path: exactly-once finalization is
    /// already enforced where the identity is exact — per processor ROW, whose
    /// `enqueued` flag is set as the row is reported and never cleared, and a
    /// row is created once per allocation (`register_finalizable`). An address
    /// reported from a never-enqueued row therefore names an object that has
    /// not been finalized.
    pub fn enqueue_unfinalized(&self, obj_addr: usize) -> bool {
        let mut queue = self.finalization_queue.lock();
        if queue.contains(obj_addr) {
            return false;
        }
        if queue.len() == FINALIZER_QUEUE_MAX_CAPACITY {
            tracing::warn!(
                "Finalizer queue passed {} pending objects; finalization is falling \
                 behind allocation (queued, not dropped)",
                FINALIZER_QUEUE_MAX_CAPACITY
            );
        }
        queue.push(obj_addr);
        true
    }

    /// Dequeue the next object for finalization. Its processor row already
    /// guarantees once-only finalization (gci i11/rp R5).
    pub fn dequeue(&self) -> Option<usize> {
        self.finalization_queue.lock().pop()
    }

    /// Relocate every queued finalizable object address through a GC pointer
    /// map. Like cleaner actions, finalizers can be deferred across GC cycles
    /// (a `finalize()` invoked while a JIT borrow is live would alias the
    /// `&mut JvmThread`), so a persisted queue entry must track object motion
    /// or `dequeue` would later hand the interpreter a freed/moved address.
    pub fn update_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        // gcg g3/rw: a `0` target is skipped, as everywhere else here.
        self.finalization_queue
            .lock()
            .remap(|addr| match pointer_map.get(&addr) {
                Some(&new) if new != 0 => new,
                _ => addr,
            });
    }

    /// Objects a [`Self::dequeue`] can hand out now: the queue less the
    /// entries [`Self::hold_until_judged`] holds back.
    pub fn pending_count(&self) -> usize {
        self.finalization_queue.lock().deliverable_len()
    }

    /// zt t31c (G1 `System.gc()`, `CRATONVM_G1_SYSTEM_GC_HOLDS_CARRIED_FINALIZABLES`):
    /// hold the queued entries among `addrs` back from [`Self::dequeue`] (and
    /// from [`Self::pending_count`]) until [`Self::release_held`], replacing
    /// any earlier hold. The door's young pause queues a dead finalizable
    /// that the same door's stop-the-world full mark must still judge as
    /// finalizer-reachable only; with the world reopened in between, the
    /// delivery thread could otherwise take it and run `finalize()` first,
    /// and a `finalize()` that stores `this` (or merely runs, with the object
    /// on its stack) makes it strongly reachable to that mark, which then
    /// keeps every weak reference into its closure set. HotSpot's one full
    /// collection never shows the object to its `Finalizer` thread before
    /// those references are cleared. Held entries stay in
    /// [`Self::pending_addresses`] (still roots) and follow a moving
    /// collection ([`Self::update_after_gc`]).
    pub fn hold_until_judged(&self, addrs: &[usize]) {
        self.finalization_queue.lock().hold(addrs);
    }

    /// Hand every held entry back to [`Self::dequeue`]. Idempotent.
    pub fn release_held(&self) {
        self.finalization_queue.lock().release();
    }

    /// Whether a non-empty hold was released since the last take (the door
    /// then wakes the delivery thread, which may have found nothing to run).
    pub fn take_hold_released(&self) -> bool {
        std::mem::take(&mut self.finalization_queue.lock().hold_released)
    }

    /// Addresses of every object still waiting for its `finalize()` to run.
    ///
    /// These MUST be handed to the collector as finalizable roots. Once
    /// `ReferenceProcessor::mark_finalizer_enqueued` flags an entry,
    /// `finalizer_referent_addresses` stops reporting it (that flag exists to
    /// stop the resurrection channel re-enqueuing the same object on every
    /// later cycle), so from that moment the only thing still referring to the
    /// object is this queue — and it holds a *raw address*, which no marker
    /// can see. `update_after_gc` already covers a moving collection, but a
    /// non-moving young mark-sweep just frees the now-unreachable object and
    /// leaves the queued address dangling; `run_finalizers` then dereferences
    /// it (`heap.class_id_of`) and faults. Keeping them alive is what JLS
    /// §12.6 requires anyway: `finalize()` runs *on* the object and may even
    /// resurrect it.
    pub fn pending_addresses(&self) -> Vec<usize> {
        self.finalization_queue.lock().addresses()
    }

    /// Get the per-finalizer timeout in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        FINALIZER_TIMEOUT_MS
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn start(&self) {
        self.running.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

impl Default for FinalizerThread {
    fn default() -> Self {
        Self::new()
    }
}
