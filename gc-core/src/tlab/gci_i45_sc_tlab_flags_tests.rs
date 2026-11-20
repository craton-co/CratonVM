use super::*;

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

fn aligned_buffer(bytes: usize) -> (Vec<u8>, *mut u8, usize) {
    let mut buf = vec![0u8; bytes + 8];
    let raw = buf.as_mut_ptr();
    let base = ((raw as usize + 7) & !7) as *mut u8;
    (buf, base, bytes)
}

fn sizing(
    use_tlab: Option<bool>,
    resize: Option<bool>,
    size: Option<u64>,
    min: Option<u64>,
    waste: Option<u64>,
    fraction: Option<u64>,
    weight: Option<u64>,
) -> TlabSizing {
    TlabSizing::from_hotspot_flags(use_tlab, resize, size, min, waste, fraction, weight)
}

/// Nothing typed is this file's constants, and the sizer's padding holds
/// the four copied flags without growing the hot `Tlab`.
#[test]
fn gci_i45_sc_nothing_typed_is_the_old_sizing() {
    let d = sizing(None, None, None, None, None, None, None);
    assert_eq!(d, TlabSizing::DEFAULT);
    assert_eq!(d, TlabSizing::default());
    assert!(d.is_default());
    assert!(d.use_tlab() && d.resize_tlab());
    assert_eq!(d.initial_size(), initial_refill_size());
    assert_eq!(d.min_size(), min_tlab_size());
    assert_eq!(d.target_refills(), TLAB_TARGET_REFILLS);
    assert_eq!(d.refill_waste_fraction() as usize, TLAB_REFILL_WASTE_FRACTION);
    assert_eq!(u32::from(d.allocation_weight()), TLAB_ALLOCATION_WEIGHT_PERCENT);
    // The ladder arithmetic the refill site had.
    assert_eq!(d.ladder_refill_request(true, || unreachable!()), initial_refill_size());
    assert_eq!(d.ladder_refill_request(false, || 4 * KIB), MIN_TLAB_SIZE);
    assert_eq!(d.ladder_refill_request(false, || 64 * KIB), 64 * KIB);
    // `TLABSize=0` is HotSpot's "ergonomic": the default start.
    assert!(sizing(None, None, Some(0), None, None, None, None).is_default());
    assert!(std::mem::size_of::<TlabShareSizer>() <= 56);
    #[cfg(target_arch = "x86_64")]
    assert_eq!(std::mem::size_of::<Tlab>(), 272, "Tlab grew");
}

/// HotSpot's readings of each value: `TLABSize` the start (clamped to the
/// minimum and the maximum, on the grid), `MinTLABSize` the floor,
/// `target_refills = max(100 / (2 * pct), 2)`, ranges clamped.
#[test]
fn gci_i45_sc_each_flag_reads_as_hotspot_reads_it() {
    let s = sizing(None, None, Some(64 * 1024), None, None, None, None);
    assert_eq!((s.initial_size(), s.min_size()), (64 * KIB, MIN_TLAB_SIZE));
    let s = sizing(None, None, Some(1001), Some(4096), None, None, None);
    assert_eq!((s.initial_size(), s.min_size()), (4 * KIB, 4 * KIB), "start >= floor");
    let s = sizing(None, None, Some(8 * 1024 * 1024), None, None, None, None);
    assert_eq!(s.initial_size(), MAX_TLAB_SIZE);
    let s = sizing(None, None, Some(12_345), None, None, None, None);
    assert_eq!(s.initial_size(), 12_352, "on the 8-byte grid");
    let tiny = sizing(None, None, None, Some(1), None, None, None);
    assert_eq!(tiny.min_size(), FRAG_TLAB_FLOOR);
    let big_min = sizing(None, None, None, Some(2 * 1024 * 1024), None, None, None);
    assert_eq!((big_min.min_size(), big_min.initial_size()), (MAX_TLAB_SIZE, MAX_TLAB_SIZE));
    let t = |p: u64| sizing(None, None, None, None, Some(p), None, None).target_refills();
    assert_eq!((t(1), t(5), t(10), t(25), t(26), t(100), t(0)), (50, 10, 5, 2, 2, 2, 50));
    let f = sizing(None, None, None, None, None, Some(u64::MAX), Some(101));
    assert_eq!((f.refill_waste_fraction(), f.allocation_weight()), (u32::MAX, 100));
    let one = sizing(None, None, None, None, None, Some(0), None);
    assert_eq!(one.refill_waste_fraction(), 1);
    assert!(!sizing(Some(false), None, None, None, None, None, None).use_tlab());
    // `-XX:-ResizeTLAB` freezes the ladder at the start.
    let frozen = sizing(None, Some(false), Some(32 * 1024), None, None, None, None);
    assert_eq!(frozen.ladder_refill_request(false, || unreachable!()), 32 * KIB);
    assert_eq!(frozen.ladder_refill_request(true, || unreachable!()), 32 * KIB);
    // A typed floor lifts the ladder's answer.
    let floor = sizing(None, None, None, Some(16 * 1024), None, None, None);
    assert_eq!(floor.ladder_refill_request(false, || 8 * KIB), 16 * KIB);
}

/// The default sizing answers exactly what `share_refill_request` does.
#[test]
fn gci_i45_sc_the_default_sizing_is_the_old_share_sizer() {
    let mut a = Tlab::empty_heap_staging();
    let mut b = Tlab::empty_heap_staging();
    let steps: [(u64, usize); 6] =
        [(7, 0), (7, 0), (8, 25 * MIB), (12, 4 * MIB), (12, 0), (13, 300 * KIB)];
    for (epoch, bytes) in steps {
        a.note_external_allocation(bytes);
        b.note_external_allocation(bytes);
        assert_eq!(
            a.share_refill_request(epoch),
            b.share_refill_request_sized(epoch, &TlabSizing::DEFAULT)
        );
        assert_eq!(a.share_sizer(), b.share_sizer());
    }
}

/// `TLABSize` opens the first window; `TLABWasteTargetPercent` divides the
/// average; `MinTLABSize` floors it; `TLABAllocationWeight` weighs it.
#[test]
fn gci_i45_sc_the_share_sizer_reads_the_vms_flags() {
    let s = sizing(None, None, Some(64 * 1024), Some(4096), Some(10), Some(16), Some(100));
    let mut tlab = Tlab::empty_heap_staging();
    assert_eq!(tlab.share_refill_request_sized(1, &s), 64 * KIB, "TLABSize");
    assert_eq!(tlab.share_sizer().refill_waste_limit(), 64 * KIB / 16);
    tlab.note_external_allocation(MIB);
    // 1 MiB per cycle / target_refills 5.
    assert_eq!(tlab.share_refill_request_sized(2, &s), (MIB / 5) & !7);
    // Weight 100: the newest sample IS the average. An idle cycle: the
    // typed 4 KiB floor, not the default 8 KiB.
    assert_eq!(tlab.share_refill_request_sized(3, &s), 4 * KIB);
    assert_eq!(tlab.share_sizer().average_bytes_per_cycle(), 0.0);
    // The default flags on the same history: a fiftieth, and the idle
    // cycle weighs 50 % (the warm-up weight beats 35 %): 512 KiB / 50.
    let mut dflt = Tlab::empty_heap_staging();
    dflt.share_refill_request(1);
    dflt.note_external_allocation(MIB);
    assert_eq!(dflt.share_refill_request(2), (MIB / 50).max(MIN_TLAB_SIZE) & !7);
    assert_eq!(dflt.share_refill_request(3), 10_480);
}

/// `-XX:-ResizeTLAB`: the start stays across collections and the
/// underpredicted raise never fires.
#[test]
fn gci_i45_sc_resize_off_freezes_the_desired_size() {
    let s = sizing(None, Some(false), Some(16 * 1024), None, None, None, None);
    let mut tlab = Tlab::empty_heap_staging();
    assert_eq!(tlab.share_refill_request_sized(1, &s), 16 * KIB);
    tlab.note_external_allocation(25 * MIB);
    assert_eq!(tlab.share_refill_request_sized(2, &s), 16 * KIB, "sampled, not resized");
    assert!(tlab.share_sizer().average_bytes_per_cycle() > 0.0);
    for _ in 0..(4 * TLAB_TARGET_REFILLS) {
        tlab.note_share_refill(16 * KIB);
        assert_eq!(tlab.share_refill_request_sized(2, &s), 16 * KIB, "no raise");
    }
    // With resizing on, the same window is raised after 2 x target refills.
    let on = sizing(None, None, Some(16 * 1024), None, None, None, None);
    let mut grows = Tlab::empty_heap_staging();
    grows.share_refill_request_sized(1, &on);
    for _ in 0..(2 * TLAB_TARGET_REFILLS) {
        grows.note_share_refill(16 * KIB);
    }
    assert_eq!(grows.share_refill_request_sized(1, &on), 32 * KIB);
}

/// `TLABRefillWasteFraction` sets the limit and its raise per keep.
#[test]
fn gci_i45_sc_the_refill_waste_limit_reads_the_fraction() {
    let s = sizing(None, None, None, None, None, Some(16), None);
    let (_owner, base, usable) = aligned_buffer(64 * 1024);
    // SAFETY: `_owner` keeps the zeroed, 8-aligned buffer alive.
    let mut tlab = unsafe { Tlab::new(base, usable) };
    tlab.tuning.share_sizer = true;
    let mut sized = TlabShareSizer {
        window_epoch: 0,
        ..TlabShareSizer::default()
    };
    sized.adopt_flags(&s);
    sized.set_desired(64 * KIB);
    assert_eq!(sized.refill_waste_limit(), 4 * KIB, "64 KiB / 16");
    tlab.adopt_share_sizer(sized);
    tlab.alloc(usable - 16 * KIB, 8).unwrap();
    let mut keeps = 0;
    while tlab.keep_on_miss(32 * KIB) {
        keeps += 1;
        assert!(keeps < 16);
    }
    // 4, 8, 12 KiB are below the 16 KiB tail; 16 KiB is not.
    assert_eq!(keeps, 3);
}

/// gcj j2/yg2 (`CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`,
/// `gci-i45sc-proposal-tlab-refill-waste-limit-per-fill`): under HotSpot's
/// limit a keep raises it by `TLABWasteIncrement` words (`0` never raises
/// it), and every sized request re-arms it to `desired /
/// TLABRefillWasteFraction`; the default sizing keeps this VM's raise and
/// does not re-arm inside a window.
#[test]
fn gcj_j2yg2_the_hotspot_waste_limit_raises_by_the_increment_and_rearms() {
    let plain = sizing(None, None, None, None, None, Some(16), None);
    let armed = |increment: u64| {
        sizing(None, None, None, None, None, Some(16), None)
            .with_hotspot_waste_limit(Some(increment))
    };
    assert!(!plain.hotspot_waste_limit());
    assert!(armed(128).hotspot_waste_limit());
    assert_eq!(armed(128).waste_increment_words(), 128);
    assert_eq!(
        sizing(None, None, None, None, None, None, None)
            .with_hotspot_waste_limit(None)
            .waste_increment_words(),
        4,
        "HotSpot's default"
    );
    let (_owner, base, usable) = aligned_buffer(64 * 1024);
    // SAFETY: `_owner` keeps the zeroed, 8-aligned buffer alive.
    let mut tlab = unsafe { Tlab::new(base, usable) };
    tlab.tuning.share_sizer = true;
    let mut sized = TlabShareSizer {
        window_epoch: 0,
        ..TlabShareSizer::default()
    };
    sized.adopt_flags(&plain);
    sized.set_desired(64 * KIB);
    tlab.adopt_share_sizer(sized);
    tlab.alloc(usable - 16 * KIB, 8).unwrap();
    let s = armed(128);
    let mut keeps = 0;
    while tlab.keep_on_miss_sized(32 * KIB, &s) {
        keeps += 1;
        assert!(keeps < 64);
    }
    // 1 KiB (128 words) per keep from 4 KiB: 4, 5, ..., 15 KiB are below
    // the 16 KiB tail (the default raise, 4 KiB, keeps 3 times).
    assert_eq!(keeps, 12);
    assert_eq!(tlab.share_sizer().refill_waste_limit(), 16 * KIB);

    // The default sizing does not re-arm inside the window ...
    let _ = tlab.share_refill_request_sized(0, &plain);
    assert_eq!(tlab.share_sizer().refill_waste_limit(), 16 * KIB);
    // ... HotSpot's re-arms at every refill.
    assert_eq!(tlab.share_refill_request_sized(0, &s), 64 * KIB);
    assert_eq!(tlab.share_sizer().refill_waste_limit(), 4 * KIB, "64 KiB / 16");

    // `TLABWasteIncrement=0`: a keep raises nothing.
    let zero = armed(0);
    assert!(tlab.keep_on_miss_sized(32 * KIB, &zero));
    assert!(tlab.keep_on_miss_sized(32 * KIB, &zero));
    assert_eq!(tlab.share_sizer().refill_waste_limit(), 4 * KIB);
}

/// gcj j3/yg3 (`CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`): the pure step
/// and threshold. The refill rule is `(1, 2 x target)`; the byte rule's
/// threshold is the same count of `desired`-sized grants, in KiB.
#[test]
fn gcj_j3yg3_the_raise_step_and_threshold_rule() {
    assert_eq!(share_raise_step_and_threshold(false, 8 * KIB, 64 * KIB, 50), (1, 100));
    assert_eq!(share_raise_step_and_threshold(true, 8 * KIB, 64 * KIB, 50), (8, 6400));
    assert_eq!(share_raise_step_and_threshold(true, 100, 64 * KIB, 50).0, 1, "at least 1");
    assert_eq!(share_raise_step_and_threshold(true, 8 * KIB, 0, 50).1, 1, "at least 1");
    assert_eq!(
        share_raise_step_and_threshold(true, usize::MAX, usize::MAX, 50),
        (u32::MAX, 100 * (usize::MAX >> 10)),
        "the step saturates; the KiB threshold of a whole-space grant still fits"
    );
}

/// gcj j3/yg3: with `desired` = 64 KiB, 100 grants of 8 KiB raise under
/// the refill rule but not under the byte rule, which raises after the
/// window's granted bytes reach 100 x 64 KiB; 100 full grants raise under
/// both. The default sizing never sets the byte rule.
#[test]
fn gcj_j3yg3_short_grants_raise_only_by_their_bytes_under_the_byte_rule() {
    let plain = sizing(None, None, None, None, None, None, None);
    assert!(!plain.raise_by_granted_bytes());
    assert!(!TlabSizing::DEFAULT.raise_by_granted_bytes());
    let bytes = plain.with_raise_by_granted_bytes();
    assert!(bytes.raise_by_granted_bytes());
    let fresh = || {
        let mut tlab = Tlab::empty_heap_staging();
        let mut sized = TlabShareSizer {
            window_epoch: 0,
            ..TlabShareSizer::default()
        };
        sized.adopt_flags(&plain);
        sized.set_desired(64 * KIB);
        tlab.adopt_share_sizer(sized);
        tlab
    };
    let target = 2 * plain.target_refills();
    // Refill rule: 100 short grants double `desired`.
    let mut t = fresh();
    for _ in 0..target {
        t.note_share_refill_sized(8 * KIB, &plain);
    }
    assert_eq!(t.share_sizer().desired, 128 * KIB, "refill rule raises");
    // Byte rule: 100 short grants are 1/8 of the bytes: no raise ...
    let mut t = fresh();
    for _ in 0..target {
        t.note_share_refill_sized(8 * KIB, &bytes);
    }
    assert_eq!(t.share_sizer().desired, 64 * KIB, "byte rule waits");
    // ... until the window's grants reach 100 x 64 KiB.
    for _ in 0..(7 * target - 1) {
        t.note_share_refill_sized(8 * KIB, &bytes);
    }
    assert_eq!(t.share_sizer().desired, 64 * KIB, "one grant short");
    t.note_share_refill_sized(8 * KIB, &bytes);
    assert_eq!(t.share_sizer().desired, 128 * KIB, "raised on the bytes");
    // Full grants: both rules raise after `target` of them.
    for rule in [plain, bytes] {
        let mut t = fresh();
        for _ in 0..(target - 1) {
            t.note_share_refill_sized(64 * KIB, &rule);
        }
        assert_eq!(t.share_sizer().desired, 64 * KIB);
        t.note_share_refill_sized(64 * KIB, &rule);
        assert_eq!(t.share_sizer().desired, 128 * KIB, "{rule:?}");
    }
}
