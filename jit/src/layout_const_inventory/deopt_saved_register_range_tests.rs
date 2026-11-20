use crate::FrameLayout;

/// The geometry both backends publish, as the emitters write it:
/// `gpr[r] -> [rbp - (base - r*8)]` for `r` in `0..16` and
/// `xmm[n] -> [rbp - (base - 128 - n*8)]` for `n` in `0..16`, over one
/// 256-byte region whose DEEPEST offset is `base`. Since round 9 wave 7b
/// (wire7b) the single-pass producer places that region directly after the
/// blind spill (`xmm[15]` at `base - 248 == reg_spill_hi`) and the outgoing
/// reserve starts one slot past `gpr[0]` (`outgoing_lo == base + 8`); this
/// fixture mirrors that.
fn layout_with_deopt_regs(base: i32) -> FrameLayout {
    FrameLayout {
        reg_spill_lo: base - 248 - 112,
        reg_spill_hi: base - 248,
        outgoing_lo: base + 8,
        deopt_gpr_lo: base - 15 * 8,
        deopt_gpr_hi: base + 8,
        deopt_xmm_lo: base - 248,
        deopt_xmm_hi: base - 120,
        frame_size: base + 64,
        ..Default::default()
    }
}

/// Every slot the deopt stub writes is classified, and the two halves do
/// not overlap. Written as a sweep rather than as endpoint assertions
/// because the failure this guards against is an off-by-one-slot at the
/// seam, where `gpr[15]` and `xmm[0]` meet.
#[test]
fn the_two_halves_partition_the_saved_registers_region() {
    let base = 848;
    let l = layout_with_deopt_regs(base);
    for r in 0..16i32 {
        let off = base - r * 8;
        assert!(
            l.is_deopt_saved_gpr_image(off),
            "gpr[{r}] at off={off} must be in the GPR half",
        );
        assert!(
            !l.is_deopt_saved_xmm_image(off),
            "gpr[{r}] at off={off} must not also be in the XMM half",
        );
        assert_eq!(l.region_name(off), "deopt-saved-gpr-image");
    }
    for n in 0..16i32 {
        let off = base - 128 - n * 8;
        assert!(
            l.is_deopt_saved_xmm_image(off),
            "xmm[{n}] at off={off} must be in the XMM half",
        );
        assert!(
            !l.is_deopt_saved_gpr_image(off),
            "xmm[{n}] at off={off} must not also be in the GPR half",
        );
        assert_eq!(l.region_name(off), "deopt-saved-xmm-image");
    }
}

/// The slot immediately outside each end is NOT claimed. The region is a
/// licence to treat words differently -- the GPR half gets a WRITE and the
/// XMM half gets its pin dropped -- so a range that runs one slot long
/// reaches storage neither argument covers.
#[test]
fn neither_half_claims_a_slot_outside_the_region() {
    let base = 848;
    let l = layout_with_deopt_regs(base);
    assert!(
        !l.is_deopt_saved_gpr_image(base + 8),
        "one slot deeper than gpr[0]"
    );
    assert!(!l.is_deopt_saved_xmm_image(base + 8));
    assert!(
        !l.is_deopt_saved_xmm_image(base - 256),
        "one slot past xmm[15]"
    );
    assert!(!l.is_deopt_saved_gpr_image(base - 256));
    // Deliberately NOT asserted: which name `region_name` gives the slot
    // one past `xmm[15]` (`base - 256`). With the wave-7b placement that is
    // the blind spill's deepest word; pinning it here would be asserting
    // the arithmetic of an unrelated reservation.
}

/// A frame that reserved no `SavedRegisters` region publishes `(0, 0)`, and
/// both predicates must read that as "absent" rather than as a range
/// containing 0 -- offset 0 is `[rbp]`, the saved caller frame pointer.
#[test]
fn an_unreserved_region_claims_nothing() {
    let l = FrameLayout {
        reg_spill_lo: 64,
        reg_spill_hi: 176,
        outgoing_lo: 176,
        ..Default::default()
    };
    for off in [0, 8, 64, 176, 400, 848] {
        assert!(!l.is_deopt_saved_gpr_image(off), "off={off}");
        assert!(!l.is_deopt_saved_xmm_image(off), "off={off}");
    }
    assert_eq!(l.region_name(400), "outgoing-args-or-deopt-regs");
}
