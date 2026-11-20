//! Round 9 wave 7 (review7b): the published `FrameLayout` must not put any
//! slot of the frame-deopt `SavedRegisters` image inside the outgoing
//! reserve, which the conservative scanner may skip as dead.

/// `gpr[0]` (the RAX image) is the block's DEEPEST slot, at
/// `[rbp - deopt_regs_base]`. The outgoing reserve therefore starts one slot
/// past it; publishing `deopt_regs_base` itself classified the saved RAX as
/// outgoing-argument storage.
#[test]
fn the_outgoing_reserve_starts_past_the_deepest_saved_register() {
    let c = super::super::emit::tests::test_compiler();
    let l = c.frame_layout();
    if l.deopt_gpr_hi == 0 {
        // No `SavedRegisters` block in this configuration
        // (`CRATONVM_DEOPT_REAL=0`): nothing to classify.
        return;
    }
    let gpr0 = l.deopt_gpr_hi - 8;
    assert!(
        l.is_deopt_saved_gpr_image(gpr0),
        "gpr[0] at {gpr0} must be classified as the saved GPR image"
    );
    assert!(
        l.outgoing_lo >= l.deopt_gpr_hi,
        "outgoing_lo {} overlaps the saved GPR image [{}, {})",
        l.outgoing_lo,
        l.deopt_gpr_lo,
        l.deopt_gpr_hi
    );
    assert!(
        gpr0 < l.outgoing_lo,
        "the saved RAX image at {gpr0} would read as dead outgoing reserve"
    );
}

/// Round 9 wave 7b (wire7b):
/// `single-pass-deopt-register-image-sits-one-slot-too-deep-20260918.md`.
/// `Compiler::new` placed `gpr[0]` one slot too deep, on the first
/// outgoing-argument word (`[RSP + 40]`, the Win64 6th-argument slot, when
/// the frame total is already 16-aligned), and left the block's shallowest
/// slot unused. The block must sit wholly above the 48-byte outgoing area
/// (32 bytes of shadow space + the 16-byte stack-argument reserve) and abut
/// the blind spill with no gap. The second assertion fails on the old
/// placement whatever the frame's alignment padding is.
#[test]
fn the_saved_register_image_stays_above_the_outgoing_area() {
    let c = super::super::emit::tests::test_compiler();
    let l = c.frame_layout();
    if l.deopt_gpr_hi == 0 {
        return;
    }
    let gpr0 = l.deopt_gpr_hi - 8;
    assert!(
        gpr0 <= l.frame_size - 48,
        "gpr[0] at {gpr0} is inside the outgoing area (frame_size {})",
        l.frame_size
    );
    assert_eq!(
        l.deopt_xmm_lo, l.reg_spill_hi,
        "the SavedRegisters block must start at the first slot past the blind spill"
    );
}
