    use super::{
        age_clock_advance, clock_promotion_age, serial_eden_bytes, should_tenure,
        sweep_survivor_mark, sweep_survivor_mark_by, young_trigger_percent, AgeClock,
        GenerationalHeap,
    };
    use cratonvm_types::{ClassId, ObjectHeader, ObjectRef, Value, GC_FLAG_MARKED, MAX_GC_AGE};
    use std::sync::atomic::Ordering;

    const M: usize = 1 << 20;

    /// One step per whole eden of allocation; the remainder carries over; an
    /// unarmed clock (eden 0) is step 1 every cycle; the step saturates at
    /// `MAX_GC_AGE` and the credit stays below one eden.
    #[test]
    fn gci_i1yg_the_clock_ticks_once_per_eden_of_allocation() {
        assert_eq!(age_clock_advance(0, 123, 0), (0, 1), "unarmed: every cycle ages");
        assert_eq!(age_clock_advance(0, 40 * M, 70 * M), (40 * M, 0), "half an eden: hold");
        assert_eq!(age_clock_advance(40 * M, 40 * M, 70 * M), (10 * M, 1), "the second half ticks");
        assert_eq!(age_clock_advance(10 * M, 0, 70 * M), (10 * M, 0), "nothing allocated");
        assert_eq!(age_clock_advance(0, 150 * M, 70 * M), (10 * M, 2), "two edens, two ages");
        let (credit, step) = age_clock_advance(0, 1000 * M, M);
        assert_eq!(step, MAX_GC_AGE, "saturates at the age ceiling");
        assert_eq!(credit, 0);
        assert_eq!(age_clock_advance(usize::MAX, usize::MAX, 7).1, MAX_GC_AGE, "no overflow");
        // f5c's geometry (`-Xmx256m`): the default trigger (32 MiB, half a
        // 64 MiB semi) is under half HotSpot's 68 MiB eden, so fewer than
        // every other cycle ticks.
        let eden = serial_eden_bytes(256 * M / 3);
        let (mut credit, mut ticks) = (0, 0u32);
        for _ in 0..20 {
            let (c, s) = age_clock_advance(credit, 32 * M, eden);
            credit = c;
            ticks += u32::from(s);
        }
        assert_eq!(ticks, (20 * 32 * M / eden) as u32, "640 MiB at a 32 MiB trigger");
        assert!(ticks < 15, "the f5c finalizables stay below the threshold of 15");
    }

    /// `should_tenure(age, f, clock_promotion_age(pa, s))` is exactly "the
    /// survivor reaches `pa` after aging by `s`", over every age, threshold
    /// and step; step 0 tenures only by force; step 1 is the identity.
    #[test]
    fn gci_i1yg_the_clock_promotion_age_tenures_exactly_at_age_plus_step() {
        for pa in 1..=16u8 {
            assert_eq!(clock_promotion_age(pa, 1), pa);
            for age in 0..=MAX_GC_AGE {
                assert!(!should_tenure(age, false, clock_promotion_age(pa, 0)), "{age} {pa}");
                assert!(should_tenure(age, true, clock_promotion_age(pa, 0)));
                for step in 1..=MAX_GC_AGE {
                    let want = u16::from(age) + u16::from(step) >= u16::from(pa);
                    assert_eq!(
                        should_tenure(age, false, clock_promotion_age(pa, step)),
                        want,
                        "age {age} pa {pa} step {step}"
                    );
                }
            }
        }
    }

    /// gci i27/gs (`gci-i23tn-...` item 1): `MaxTenuringThreshold=16` is
    /// `promotion_age` 17 (TN-H1) and tenures nobody by age at ANY clock step,
    /// as HotSpot's `NeverTenure` (an age saturates at 15); 16 and below are
    /// unchanged.
    #[test]
    fn gci_i27_gs_never_tenure_survives_the_age_clock() {
        let never = MAX_GC_AGE + 2;
        for step in 0..=MAX_GC_AGE {
            for age in 0..=MAX_GC_AGE {
                assert!(
                    !should_tenure(age, false, clock_promotion_age(never, step)),
                    "age {age} step {step}"
                );
            }
        }
        assert_eq!(clock_promotion_age(16, 2), 15, "15 is still reachable");
        // The threshold store accepts 16 and the decision keeps it.
        let st = super::tenuring::TenuringState::new();
        st.configure(&super::TenuringConfig {
            max_tenuring_threshold: Some(16),
            ..Default::default()
        });
        assert_eq!(st.promotion_age(false, 3), never);
        let d = st.adapt(&[0; super::tenuring::AGE_TABLE_LEN], 1 << 20);
        assert_eq!((d.next, d.max), (16, 16), "max threshold 16, as -Xlog:gc+age prints");
    }

    /// Step 1 is the historical sweep word bit for bit; step 0 clears the
    /// mark and keeps the age.
    #[test]
    fn gci_i1yg_the_sweep_word_ages_by_the_step() {
        for age in 0..=MAX_GC_AGE {
            for marked in [false, true] {
                let base = ObjectHeader::mark_with_gc_age(0, age);
                let m = if marked {
                    ObjectHeader::mark_with_gc_flags(base, GC_FLAG_MARKED)
                } else {
                    base
                };
                assert_eq!(sweep_survivor_mark_by(m, 1), sweep_survivor_mark(m));
                let (word, new_age) = sweep_survivor_mark_by(m, 0);
                assert_eq!(new_age, age);
                assert_eq!(word, base, "mark cleared, age kept");
                let (_, two) = sweep_survivor_mark_by(m, 2);
                assert_eq!(two, age.saturating_add(2).min(MAX_GC_AGE));
            }
        }
    }

    /// The switch arms the clock on an `-Xmx` heap with HotSpot's eden and
    /// leaves the TRIGGER where the percent knob puts it (that is the point:
    /// the Serial trigger's cost stays off); adaptive tenuring's survivor
    /// target becomes HotSpot's. Off (unset or `=0`): unarmed.
/// `age_clock_begin` charges the young live bytes since the last
    /// collection; a refused cycle gives its allocation back without a tick;
    /// a completed one records what young holds as the next base.
    #[test]
    fn gci_i1yg_begin_and_end_charge_young_allocation() {
        let mut heap = GenerationalHeap::with_sizes(4 * M, 4 * M);
        heap.young_age_clock_eden = M;
        for i in 0..8000 {
            let o = heap.alloc_object(ClassId::new(1), 30);
            heap.set_field(o, 0, Value::Int(i));
        }
        let live = {
            let (used, free, _, _) = heap.young_from_occupancy();
            used.saturating_sub(free)
        };
        assert!(live > 0);
        heap.age_clock_begin();
        let c = *heap.age_clock.lock();
        let (credit, step) = age_clock_advance(0, live, M);
        assert_eq!((c.credit, c.step, c.entry_live), (credit, step, live));
        assert_eq!(heap.age_clock_step(), step);

        heap.young_cycle_skipped.store(true, Ordering::Relaxed);
        heap.age_clock_end();
        let c = *heap.age_clock.lock();
        assert_eq!((c.credit, c.exit_live), (live, live), "refused: refunded, no tick");
        assert_eq!((c.ticks, c.holds), (0, 0));

        heap.young_cycle_skipped.store(false, Ordering::Relaxed);
        heap.age_clock_begin();
        let c = *heap.age_clock.lock();
        assert_eq!((c.credit, c.step), age_clock_advance(live, 0, M), "nothing new allocated");
        heap.age_clock_end();
        let c = *heap.age_clock.lock();
        assert_eq!(c.exit_live, live, "young unchanged by a cycle that did not run");
        assert_eq!(c.ticks + c.holds, 1);
        assert_eq!(AgeClock::default().step, 1, "a cycle the clock never saw ages by one");
    }

    /// Through the real non-moving sweep: with an eden no cycle reaches, no
    /// survivor ages and none tenures by age (the child would be promoted at
    /// `PROMOTION_AGE` = 3 otherwise -- the control), and every cycle is a
    /// hold.
    #[test]
    fn gci_i1yg_sweeps_below_one_eden_neither_age_nor_tenure() {
        struct NoMonitors;
        impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
            fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
        }
        let run = |clock_eden: usize| -> (u8, bool, u64) {
            let mut heap =
                GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
            heap.young_age_clock_eden = clock_eden;
            let holder = heap.alloc_object(ClassId::new(1), 2);
            let child = heap.alloc_object(ClassId::new(1), 1);
            heap.set_field(child, 0, Value::Int(0x5EED));
            heap.set_field(holder, 0, Value::Object(Some(child)));
            let mut roots: Vec<ObjectRef> = vec![holder];
            for _ in 0..6 {
                cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(false);
                cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
                cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
                cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
                cratonvm_gc_core::gc_quiescence::enter();
                // SAFETY: this test runs the heap single-threaded.
                let token = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
                let _ = heap.collect_garbage(&token, &mut roots, &NoMonitors);
                cratonvm_gc_core::gc_quiescence::leave();
            }
            let c = match heap.get_field(roots[0], 0) {
                Value::Object(Some(c)) => c,
                other => panic!("the holder lost its child: {other:?}"),
            };
            assert_eq!(heap.get_field(c, 0).as_int(), Some(0x5EED));
            let holds = heap.young_trigger_census().age_clock_holds;
            (heap.get_header(c).gc_age(), heap.is_in_old(c.as_ptr()), holds)
        };
        let (age, old, holds) = run(usize::MAX / 4);
        assert_eq!((age, old), (0, false), "below one eden: no aging, no tenure");
        assert_eq!(holds, 6, "every cycle held");
        let (age, old, _) = run(0);
        assert!(old || age > 0, "control: the unarmed sweep ages (age {age}, old {old})");
    }
