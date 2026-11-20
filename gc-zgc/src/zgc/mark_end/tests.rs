    //! zt t23/zc6: the plan (pure), the switch reader, the census on a real
    //! concurrent cycle (with a positive control for a claim in the closing
    //! pause), and the service's look and ticket on a real heap.

    use super::*;
    use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
    use crate::zgc::timer::ZTickCause;
    use cratonvm_types::{ClassId, ObjectRef, Value};

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
        fn prune_candidate_keys(&self) -> Option<Vec<usize>> {
            Some(Vec::new())
        }
    }

    fn inputs(cycle: usize, caught_up: bool, sysgc_owned: bool, seen: usize) -> ZMarkEndInputs {
        ZMarkEndInputs {
            armed: true,
            cycle,
            caught_up,
            sysgc_owned,
            seen,
        }
    }

    /// The reader (default ON since zt t26/zc9): unset on; on under the
    /// profile; its own `=0` kills it, inside the profile too; `=1` on; the
    /// profile's `=0` does not turn it off (it acts only where an every-trigger
    /// cycle exists).
/// The plan: off when not armed; the idle period with no cycle; the short
    /// period while marking or while a `System.gc()` caller owns the cycle
    /// (which also forgets the sighting); a first sighting at the fixed point
    /// remembers the cycle, the second closes it; a sighting of an OLDER cycle
    /// is not this one's.
    #[test]
    fn zt23zc6_the_plan_closes_on_the_second_look_at_the_fixed_point() {
        let mut off = inputs(3, true, false, 3);
        off.armed = false;
        assert_eq!(mark_end_plan(off), ZMarkEndPlan::Off);
        assert_eq!(
            mark_end_plan(inputs(0, false, false, 7)),
            ZMarkEndPlan::Wait {
                seen: 0,
                wait: ZGC_MARK_END_IDLE_POLL
            }
        );
        assert_eq!(
            mark_end_plan(inputs(3, false, false, 3)),
            ZMarkEndPlan::Wait {
                seen: 0,
                wait: ZGC_MARK_END_POLL
            },
            "still marking: no sighting kept"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, true, 3)),
            ZMarkEndPlan::Wait {
                seen: 0,
                wait: ZGC_MARK_END_POLL
            },
            "a System.gc() caller closes its own cycle"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, false, 0)),
            ZMarkEndPlan::Wait {
                seen: 3,
                wait: ZGC_MARK_END_POLL
            },
            "first sighting"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, false, 2)),
            ZMarkEndPlan::Wait {
                seen: 3,
                wait: ZGC_MARK_END_POLL
            },
            "the previous sighting was another cycle"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, false, 3)),
            ZMarkEndPlan::Close { cycle: 3 }
        );
    }

    /// The merge: a base collection (the timer) wins over everything; a close
    /// replaces a wait or an off; waits take the shorter; off keeps the base.
    #[test]
    fn zt23zc6_the_merge_keeps_the_timer_first() {
        let close = ZMarkEndPlan::Close { cycle: 1 };
        let wait = ZMarkEndPlan::Wait {
            seen: 0,
            wait: ZGC_MARK_END_POLL,
        };
        let timer = ZTick::Collect(ZTickCause::Timer);
        assert_eq!(merge_with_base(timer, close), timer);
        assert_eq!(merge_with_base(timer, wait), timer);
        assert_eq!(merge_with_base(ZTick::Off, close), ZTick::CloseMark);
        assert_eq!(
            merge_with_base(ZTick::Wait(Duration::from_secs(1)), close),
            ZTick::CloseMark
        );
        assert_eq!(
            merge_with_base(ZTick::Wait(Duration::from_secs(1)), wait),
            ZTick::Wait(ZGC_MARK_END_POLL)
        );
        assert_eq!(
            merge_with_base(ZTick::Wait(Duration::from_micros(100)), wait),
            ZTick::Wait(Duration::from_micros(100))
        );
        assert_eq!(
            merge_with_base(ZTick::Off, wait),
            ZTick::Wait(ZGC_MARK_END_POLL)
        );
        assert_eq!(merge_with_base(ZTick::Off, ZMarkEndPlan::Off), ZTick::Off);
    }

    /// A chain `head -> n1 -> ... -> n(len-1)` built with plain stores.
    fn chain(heap: &ZgcRealHeap, len: usize) -> ObjectRef {
        let head = heap.alloc_object(ClassId::new(71), 1);
        let mut at = head;
        for _ in 1..len {
            let next = heap.alloc_object(ClassId::new(71), 1);
            heap.set_field(at, 0, Value::Object(Some(next)));
            at = next;
        }
        head
    }

    /// Wait (bounded) for the open cycle's pool to reach its fixed point.
    fn wait_caught_up(heap: &ZgcRealHeap) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !heap.concurrent_mark_caught_up() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            heap.concurrent_mark_caught_up(),
            "the pool reached its fixed point"
        );
    }

    /// The census on a real concurrent cycle. A 500-object chain is the only
    /// root at mark start and the pool traces it before the collection
    /// arrives: the closing pause re-pushes the root and claims nothing new
    /// (`claimed=0`, `end_marked=0`), the phase's mark is `conc_marked>0`, one
    /// certified close with the pool caught up at entry. Positive control: a
    /// second chain unreachable at mark start handed over as a root at the
    /// closing collection is claimed by the closing pause (`claimed>=1`) and
    /// survives.
    #[test]
    fn zt23zc6_the_census_splits_the_phase_from_the_closing_pause() {
        for late_root in [false, true] {
            let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
            heap.set_relocation_enabled(false);
            let head = chain(&heap, 500);
            let late = chain(&heap, 50);
            let late_addr = late.as_ptr() as usize;
            {
                // SAFETY: single-threaded test; the token stands for the pause.
                let stw = unsafe { StopTheWorldToken::new() };
                assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
            }
            wait_caught_up(&heap);
            {
                // SAFETY: single-threaded test.
                let stw = unsafe { StopTheWorldToken::new() };
                let mut roots: Vec<ObjectRef> = vec![head];
                if late_root {
                    roots.push(late);
                }
                let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            }
            let r = heap.mark_end_report();
            assert_eq!((r.closed, r.caught_up_at_entry, r.fallbacks), (1, 1, 0));
            assert!(r.conc_marked > 0, "late_root={late_root}: the phase marked");
            assert!(r.pushed >= 1, "the roots were pushed again");
            if late_root {
                assert!(r.claimed >= 1, "positive control: the late root is claimed");
                assert!(heap.is_object_address(late_addr).is_some(), "and kept");
            } else {
                assert_eq!(r.claimed, 0, "nothing new at the closing pause");
                assert_eq!(r.end_marked, 0);
                assert!(
                    heap.is_object_address(late_addr).is_none(),
                    "control: the unreachable chain was swept"
                );
            }
        }
    }

    /// The service's look and ticket on a real heap: not armed by default (the
    /// base tick is returned unchanged); armed, an open cycle at its fixed
    /// point is closed on the second look; a `System.gc()`-owned cycle is
    /// left alone; a ticket for a cycle another collection closed is stale,
    /// and a pause that closed nothing certified counts `extra`.
    #[test]
    fn zt23zc6_the_service_closes_a_caught_up_cycle_on_the_second_look() {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        heap.set_relocation_enabled(false);
        let base = ZTick::Wait(Duration::from_secs(1));
        assert!(!heap.mark_end_service_armed(), "default: not armed");
        assert_eq!(heap.mark_end_tick(base), base);
        heap.counters.mend.enabled.store(true, Ordering::Relaxed);
        heap.dbg_set_conc_start_bytes(heap.gc_threshold * 60 / 100);
        heap.dbg_set_conc_every_trigger(true);
        assert!(heap.mark_end_service_armed());
        assert_eq!(
            heap.mark_end_tick(ZTick::Off),
            ZTick::Wait(ZGC_MARK_END_IDLE_POLL),
            "no cycle: the idle period, and the thread stays"
        );
        let head = chain(&heap, 200);
        {
            // SAFETY: single-threaded test.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
        }
        wait_caught_up(&heap);
        assert_eq!(
            heap.mark_end_tick(base),
            ZTick::Wait(ZGC_MARK_END_POLL),
            "first look"
        );
        assert_eq!(heap.mark_end_tick(base), ZTick::CloseMark, "second look");
        let ticket = heap.mark_end_service_begin().expect("still due");
        // A `System.gc()` owner makes it not due.
        heap.counters
            .conc_trigger
            .sysgc_owner_deadline
            .store(1, Ordering::Relaxed);
        assert!(!heap.mark_end_service_still_due(ticket));
        assert_eq!(heap.mark_end_tick(base), ZTick::Wait(ZGC_MARK_END_POLL));
        heap.counters
            .conc_trigger
            .sysgc_owner_deadline
            .store(0, Ordering::Relaxed);
        assert!(heap.mark_end_service_still_due(ticket));
        {
            // SAFETY: single-threaded test; the service's pause.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [head];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        heap.mark_end_service_end(ticket, Some(true), false);
        assert!(!heap.mark_end_service_still_due(ticket), "closed");
        assert!(heap.mark_end_service_begin().is_none(), "no cycle: stale");
        // A pause whose ticket was taken after that close closed no certified
        // cycle (the race the second look narrows): `extra`.
        let late = ZMarkEndTicket {
            cycle: ticket.cycle,
            completed: heap.counters.conc_cycles_completed.load(Ordering::Relaxed),
        };
        heap.mark_end_service_end(late, Some(true), false);
        heap.mark_end_service_end(late, Some(false), false);
        // zt t31/zc11 (1f R4): another thread's release moves the global count,
        // which no longer books this attempt as continued.
        heap.counters
            .mend
            .mcc_continues
            .fetch_add(1, Ordering::Relaxed);
        heap.mark_end_service_end(late, Some(false), false);
        heap.mark_end_service_end(late, Some(false), true);
        let r = heap.mark_end_report();
        assert!(r.svc_ticks >= 3);
        assert_eq!(
            (r.svc_closes, r.svc_extra, r.svc_lost, r.svc_stale),
            (2, 1, 2, 1)
        );
        assert_eq!(
            heap.mark_continue_report().svc_continued,
            1,
            "only the attempt released on its own thread"
        );
        assert_eq!(r.closed, 1);
    }

    // ---- zt t24/zc8: Concurrent Mark Continue -----------------------------

    /// The reader (default ON since zt t26/zc9): unset on; on under the
    /// profile; its own `=0` kills it, inside the profile too; `=1` on; the
    /// profile's `=0` does not turn it off (it acts only where an every-trigger
    /// cycle exists).
/// A heap with the switch seeded on and the every-trigger mode acting.
    fn continue_heap() -> std::sync::Arc<ZgcRealHeap> {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        heap.set_relocation_enabled(false);
        heap.counters
            .mend
            .continue_enabled
            .store(true, Ordering::Relaxed);
        heap.dbg_set_conc_start_bytes(heap.gc_threshold * 60 / 100);
        heap.dbg_set_conc_every_trigger(true);
        heap
    }

    /// The attempt on a real concurrent cycle. A 300-object chain is the only
    /// root at mark start and the pool traces it. Attempt 1 is handed a second
    /// chain (40 objects) unreachable at mark start as a root -- the positive
    /// control: it claims the head (`claimed >= 1`), so the mark is not
    /// complete and the verdict is `Continue`; the pool, which had declared
    /// its fixed point, is re-armed and traces the rest of that chain with no
    /// pause (its tail is marked once the pool is caught up again). Attempt 2,
    /// same roots, claims nothing at the fixed point: `Complete`. The closing
    /// collection then closes the cycle with its mark set (one certified
    /// close, the late chain kept), and its log carries the one failed
    /// attempt with the pause the VM booked (5 us) and a continue phase.
    #[test]
    fn zt24zc8_an_attempt_with_work_left_continues_and_the_next_completes() {
        let heap = continue_heap();
        assert!(heap.mark_continue_armed());
        let head = chain(&heap, 300);
        // The late chain, built by hand so its tail is known.
        let late = heap.alloc_object(ClassId::new(71), 1);
        let mut tail = late;
        for _ in 1..40 {
            let next = heap.alloc_object(ClassId::new(71), 1);
            heap.set_field(tail, 0, Value::Object(Some(next)));
            tail = next;
        }
        let (late_addr, tail_addr) = (late.as_ptr() as usize, tail.as_ptr() as usize);
        {
            // SAFETY: single-threaded test; the token stands for the pause.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
        }
        wait_caught_up(&heap);
        assert!(
            !heap.mark_is_set(late_addr),
            "control: unreachable at mark start"
        );
        assert!(
            heap.mark_continue_wanted(),
            "an open cycle, nothing used up"
        );
        let roots = [head, late];
        let first = {
            // SAFETY: single-threaded test; the attempt's pause.
            let stw = unsafe { StopTheWorldToken::new() };
            heap.try_mark_end(&stw, &roots)
        };
        assert_eq!(first, ZMarkEndAttempt::Continue);
        heap.mark_continue_released(5_000);
        // The cycle is still open and the barrier still armed: an attempt
        // closes nothing.
        assert!(heap.conc_cycle_active.load(Ordering::Acquire));
        assert!(heap.mark_active());
        wait_caught_up(&heap);
        assert!(
            heap.mark_is_set(late_addr) && heap.mark_is_set(tail_addr),
            "traced concurrently"
        );
        let second = {
            // SAFETY: single-threaded test.
            let stw = unsafe { StopTheWorldToken::new() };
            heap.try_mark_end(&stw, &roots)
        };
        assert_eq!(second, ZMarkEndAttempt::Complete);
        {
            // SAFETY: single-threaded test; the closing collection.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [head, late];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        assert!(heap.is_object_address(late_addr).is_some(), "kept");
        assert!(heap.is_object_address(tail_addr).is_some(), "kept");
        let me = heap.mark_end_report();
        assert_eq!((me.closed, me.fallbacks), (1, 0), "one certified close");
        let r = heap.mark_continue_report();
        assert_eq!((r.attempts, r.continues, r.completes), (2, 1, 1));
        assert!(
            r.claimed >= 1,
            "positive control: the late root was claimed"
        );
        assert!(r.pushed >= 4, "both attempts handed both roots");
        assert_eq!((r.pause_us, r.max_cycle_continues), (5, 1));
        let log = heap.take_mark_continue_log();
        assert_eq!(log.len(), 1, "one failed attempt in the closed cycle");
        assert_eq!(log[0].0, 5_000);
        assert!(heap.take_mark_continue_log().is_empty(), "taken");
        assert_eq!(
            heap.counters.mend.cycle_continues.load(Ordering::Relaxed),
            0,
            "the close restarts the per-cycle count"
        );
    }

    /// When an attempt is not due: not armed by default (the base path, the
    /// census untouched); armed with no cycle open; a cycle that used its
    /// attempts (`exhausted`); free space at the urgent reserve (`urgent`).
    /// Control: armed with an open cycle, it is due.
    #[test]
    fn zt24zc8_an_attempt_is_due_only_on_an_open_cycle_with_room() {
        let plain = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        plain.set_relocation_enabled(false);
        assert!(!plain.mark_continue_armed(), "default: not armed");
        assert!(!plain.mark_continue_wanted());
        assert!(plain.take_mark_continue_log().is_empty());
        let heap = continue_heap();
        assert!(!heap.mark_continue_wanted(), "no cycle open");
        let head = chain(&heap, 50);
        {
            // SAFETY: single-threaded test.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
        }
        assert!(heap.mark_continue_wanted(), "control: due");
        let st = &heap.counters.mend;
        st.cycle_continues
            .store(ZGC_MARK_CONTINUE_MAX_ATTEMPTS, Ordering::Relaxed);
        assert!(!heap.mark_continue_wanted(), "the cycle used its attempts");
        st.cycle_continues.store(0, Ordering::Relaxed);
        let allocated = heap.allocated.load(Ordering::Relaxed);
        heap.allocated
            .store(heap.heap_capacity(), Ordering::Relaxed);
        assert!(!heap.mark_continue_wanted(), "at the urgent reserve");
        heap.allocated.store(allocated, Ordering::Relaxed);
        assert!(heap.mark_continue_wanted(), "due again");
        let r = heap.mark_continue_report();
        assert_eq!((r.exhausted, r.urgent, r.attempts), (1, 1, 0));
        heap.abandon_concurrent_mark();
    }

    /// Stage 0 CPU: a concurrent close forgets a constrained pause-target
    /// budget (span, overrun memory, the allocation trigger back to the
    /// percentage form); a stop-the-world close (the control) does not; the
    /// kill (`=0`, seeded off) does not.
    #[test]
    fn zt24zc8_a_concurrent_close_forgets_the_pause_budget() {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        let constrain = |h: &ZgcRealHeap| {
            h.pause_affordable_span.store(9 << 20, Ordering::Relaxed);
            h.pause_overrun_span.store(12 << 20, Ordering::Relaxed);
            h.alloc_trigger_bytes.store(3 << 20, Ordering::Relaxed);
        };
        let mark = |h: &ZgcRealHeap, concurrent: bool| {
            h.counters
                .last_collection_marked_concurrently
                .store(concurrent, Ordering::Relaxed);
        };
        assert!(
            heap.counters.mend.budget_forget.load(Ordering::Relaxed),
            "default ON"
        );
        constrain(&heap);
        mark(&heap, false);
        assert!(
            !heap.conc_close_forgets_pause_budget(),
            "control: a stop-the-world close"
        );
        assert_eq!(heap.alloc_trigger_bytes.load(Ordering::Relaxed), 3 << 20);
        mark(&heap, true);
        assert!(heap.conc_close_forgets_pause_budget());
        assert_eq!(heap.pause_affordable_span.load(Ordering::Relaxed), 0);
        assert_eq!(heap.pause_overrun_span.load(Ordering::Relaxed), 0);
        assert_eq!(
            heap.alloc_trigger_bytes.load(Ordering::Relaxed),
            heap.alloc_trigger_percent_bytes
        );
        assert_eq!(heap.mark_continue_report().budget_forgets, 1);
        heap.counters
            .mend
            .budget_forget
            .store(false, Ordering::Relaxed);
        constrain(&heap);
        assert!(!heap.conc_close_forgets_pause_budget(), "the kill");
        assert_eq!(heap.alloc_trigger_bytes.load(Ordering::Relaxed), 3 << 20);
    }
