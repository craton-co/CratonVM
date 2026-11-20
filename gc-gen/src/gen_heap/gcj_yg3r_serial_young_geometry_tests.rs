    use super::*;

    /// R2: the copy's workers follow the survivors, between two and the
    /// occupancy's count; off, or a single-worker ask, as given.
    #[test]
    fn gcj_yg3r_copy_workers_follow_the_survivors() {
        const MB: usize = 1 << 20;
        assert_eq!(copy_workers_for_survivors(8, 0, true), 2);
        assert_eq!(copy_workers_for_survivors(8, 3 * MB, true), 3);
        assert_eq!(copy_workers_for_survivors(8, 3 * MB + 1, true), 4);
        assert_eq!(copy_workers_for_survivors(8, 400 * MB, true), 8);
        assert_eq!(
            copy_workers_for_survivors(1, 400 * MB, true),
            1,
            "the serial ask stays"
        );
        assert_eq!(copy_workers_for_survivors(8, 0, false), 8, "=0: occupancy");
    }
    use cratonvm_gc_core::gen_young_sizing::YoungGenSizing;

    const M: usize = 1024 * 1024;

    /// HotSpot 25 Serial's own numbers (`Yg3rGeometryProbe pools`): eden and
    /// survivor maxima for `-Xmx256m` with `-Xmn128m`, with nothing typed,
    /// and with `-XX:SurvivorRatio=4`; the old generation is `Xmx - Y`.
    #[test]
    fn gcj_yg3r_the_geometry_is_hotspots_eden_survivor_and_tenured() {
        let xmn = YoungGenSizing {
            xmn: Some(128 * M),
            ..YoungGenSizing::default()
        };
        let (g, old) = serial_young_geometry_for(256 * M, &xmn, 8).expect("plans");
        assert_eq!(
            (g.eden, g.survivor, old),
            (107_479_040, 13_369_344, 134_217_728)
        );
        assert_eq!(g.semi_reserve, g.eden + g.survivor);
        let (g, old) =
            serial_young_geometry_for(256 * M, &YoungGenSizing::default(), 8).expect("plans");
        assert_eq!(
            (g.eden, g.survivor, old),
            (71_630_848, 8_912_896, 178_978_816)
        );
        let (g, _) = serial_young_geometry_for(256 * M, &xmn, 4).expect("plans");
        assert_eq!((g.eden, g.survivor), (89_522_176, 22_347_776));
        // Too small for an aligned eden and two survivors: the pair as before.
        assert!(serial_young_geometry_for(256 * 1024, &YoungGenSizing::default(), 8).is_none());
    }

    /// The Serial geometry changes the initial generation split, not the
    /// default-on old-generation borrowing contract. A refused old allocation
    /// can grow into the reserved headroom and lower the young trigger so the
    /// committed heap remains within `-Xmx`.
/// The trigger is eden on top of the carry, the carry counted up to one
    /// survivor, within 95 % of the semi.
    #[test]
    fn gcj_yg3r_the_trigger_is_eden_on_top_of_the_young_carry() {
        let (eden, s, semi) = (80 * M, 10 * M, 90 * M);
        assert_eq!(geometry_young_trigger(eden, 0, s, semi), eden);
        assert_eq!(geometry_young_trigger(eden, 3 * M, s, semi), eden + 3 * M);
        assert_eq!(
            geometry_young_trigger(eden, 40 * M, s, semi),
            semi * SERIAL_EDEN_FULL_PERCENT / 100,
            "a carry past one survivor counts as one, and 95 % of the semi caps it"
        );
        assert_eq!(
            geometry_young_trigger(eden, 5 * M, s, 84 * M),
            84 * M * 95 / 100
        );
    }

    /// The serial copy tenures a survivor that would take to-space past the
    /// cap, keeps it young under the cap or when old gen refuses, and the
    /// default queue has no cap.
    #[test]
    fn gcj_yg3r_the_serial_copy_tenures_past_the_survivor_cap() {
        let mut og = OldGen::new(M);
        let mut q = PromotionQueue::new();
        assert_eq!(
            q.to_space_cap,
            usize::MAX,
            "no cap unless the geometry sets one"
        );
        assert!(geometry_cap_destination(&mut q, &mut og, usize::MAX / 2, 64).is_none());
        q.to_space_cap = 4096;
        assert!(
            geometry_cap_destination(&mut q, &mut og, 4096 - 64, 64).is_none(),
            "fits"
        );
        let before = GEOMETRY_CAP_PROMOTIONS.load(Ordering::Relaxed);
        let p = geometry_cap_destination(&mut q, &mut og, 4096 - 56, 64).expect("tenured");
        assert!(og.contains(p as *const u8));
        assert!(GEOMETRY_CAP_PROMOTIONS.load(Ordering::Relaxed) >= before + 1);
        let mut tiny = OldGen::new(4096);
        let mut q2 = PromotionQueue::new();
        q2.to_space_cap = 0;
        assert!(
            geometry_cap_destination(&mut q2, &mut tiny, 0, M).is_none(),
            "old gen refuses: the copy stays young"
        );
        q.retire_plab(&mut og);
    }
