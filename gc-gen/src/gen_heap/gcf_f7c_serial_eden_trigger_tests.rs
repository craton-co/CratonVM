    use super::{
        gen_young_trigger_serial_eden_enabled, serial_eden_bytes, serial_eden_young_trigger,
        serial_young_gen_bytes, young_trigger_percent, GenerationalHeap,
    };
    use cratonvm_gc_core::gen_young_sizing::YoungGenSizing;

    const M: usize = 1 << 20;

    /// `(-Xmn, NewSize, MaxNewSize, NewRatio)`.
    fn sizing(
        xmn: Option<usize>,
        new_size: Option<usize>,
        max_new_size: Option<usize>,
        new_ratio: Option<usize>,
    ) -> YoungGenSizing {
        YoungGenSizing {
            xmn,
            new_size,
            max_new_size,
            new_ratio,
        }
    }

    /// HotSpot's young generation: `-Xmx / 3` by default, `-Xmn` exactly,
    /// `NewRatio`, and the `NewSize` / `MaxNewSize` clamps.
    #[test]
    fn gcf_f7c_serial_young_gen_follows_hotspot_resolution() {
        let young = |s: YoungGenSizing| serial_young_gen_bytes(256 * M, &s);
        assert_eq!(young(sizing(None, None, None, None)), 256 * M / 3);
        assert_eq!(
            young(sizing(Some(32 * M), None, None, Some(5))),
            32 * M,
            "-Xmn wins"
        );
        assert_eq!(young(sizing(None, None, None, Some(3))), 64 * M);
        assert_eq!(young(sizing(None, None, Some(16 * M), None)), 16 * M);
        assert_eq!(young(sizing(None, Some(128 * M), None, None)), 128 * M);
        // NewSize above MaxNewSize raises the ceiling to NewSize (HotSpot).
        assert_eq!(
            young(sizing(None, Some(40 * M), Some(20 * M), None)),
            40 * M
        );
        assert_eq!(
            young(sizing(Some(1024 * M), None, None, None)),
            256 * M,
            "never above -Xmx"
        );
    }

    /// Eden is `SurvivorRatio / (SurvivorRatio + 2)` of the young generation:
    /// 80 %, which at `-Xmx256m` is within 1 % of HotSpot's measured 69 376 K.
    #[test]
    fn gcf_f7c_serial_eden_is_eight_tenths_of_young() {
        assert_eq!(serial_eden_bytes(10 * M), 8 * M);
        let eden = serial_eden_bytes(256 * M / 3);
        assert!(
            eden.abs_diff(69_376 * 1024) * 100 < 69_376 * 1024,
            "eden {eden}"
        );
    }

    /// gci i30/gv (`gci-i27gs-serial-young-sizes-are-not-space-aligned-FIXED-20261016`):
    /// HotSpot Serial's aligned `Y - 2S` -- `-Xmx256m` (Y 89 456 640, S
    /// 8 912 896), `-Xms32m`'s young, `-Xmn16m` (S 1 638 400), an aligned
    /// young unchanged, and a young too small for two survivors keeps 80 %.
    #[test]
    fn gci_i30gv_serial_eden_is_hotspots_aligned_y_minus_two_s() {
        assert_eq!(serial_eden_bytes(256 * M / 3), 71_630_848);
        assert_eq!(serial_eden_bytes(32 * M / 3), 8_912_896);
        assert_eq!(serial_eden_bytes(16 * M), 13_500_416);
        assert_eq!(serial_eden_bytes(10 * M), 8 * M);
        assert_eq!(serial_eden_bytes(100 * 1024), 81_920);
        assert_eq!(serial_eden_bytes(0), 0);
        assert_eq!(serial_eden_bytes(usize::MAX) % (64 * 1024), 0);
    }

    /// The trigger is the eden capped at 95 % of the semi-space, and the cap
    /// binds at this collector's default geometry (semi = `-Xmx / 4`).
    #[test]
    fn gcf_f7c_serial_trigger_is_eden_capped_at_the_semi() {
        let semi = 256 * M / 4;
        let eden = serial_eden_bytes(256 * M / 3);
        assert!(eden > semi);
        assert_eq!(serial_eden_young_trigger(semi, eden), semi * 95 / 100);
        assert_eq!(
            serial_eden_young_trigger(64 * M, 10 * M),
            10 * M,
            "eden below the cap"
        );
        assert_eq!(serial_eden_young_trigger(0, 10 * M), 1, "never zero");
    }

    /// Opt-in (gch h4/fl flipped it and reverted it on vh4-fl B): unset and
    /// `=0` read off, `=1` reads on.
/// gcg h1/ob (`gcg-g4yg-young-sizing-and-xlog-review-findings` item 7):
    /// `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` is read per heap, so two heaps
    /// built under different values keep their own (it was a process-global
    /// `OnceLock`: the second heap saw the first one's value).


    /// gch h2/ob (`gcf-f5c-...`; the gcg h1/ob sweep cap is deleted): at the
    /// vh1-ob rows' geometry (`-Xmx256m`: semi 64 MiB, HotSpot's eden
    /// 71 630 848 bytes since gci i30/gv, trigger 63 753 420) an armed heap's SWEEP fires at
    /// 90 % of the semi -- the level nearest HotSpot Serial's eden -- and its
    /// moving cycle at the capped eden; the default arm's sweep stays at its
    /// 50 % moving threshold.
    #[test]
    fn gch_h2ob_serial_eden_sweeps_at_ninety_percent_again() {
        use super::young_gc_trigger_bytes;
        let semi = 64 * M;
        let eden = serial_eden_bytes(256 * M / 3);
        assert_eq!(eden, 71_630_848);
        let serial = serial_eden_young_trigger(semi, eden);
        assert_eq!(serial, 63_753_420);
        assert_eq!(
            young_gc_trigger_bytes(semi, serial, true, true),
            semi * 90 / 100
        );
        assert_eq!(young_gc_trigger_bytes(semi, serial, false, true), serial);
        assert_eq!(young_gc_trigger_bytes(semi, semi / 2, true, true), semi / 2);
    }

    // `=0`: a `-Xmx` heap's trigger is the percent knob's, and the census says
    // so (`serial_eden_bytes = 0`). On: the trigger is the capped eden, the
    // census carries HotSpot's eden, and adaptive tenuring sizes its survivor
    // target from that eden, not from the semi-space.
