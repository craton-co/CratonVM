//! gen r4w6/oomjit6: which frame words a local-handler hit may zero under
//! the opt-in `CRATONVM_JIT_LOCAL_HANDLER_CLEAR_DEAD=1`. The set must be exactly the
//! operand-spill words above the handler's `[exception]` slot plus the
//! outgoing reserve -- never a local, a hoist, a scalar-replaced field, a
//! register image or the deopt `SavedRegisters` block.

use super::dead_words_at_handler_entry;

#[test]
fn a_synthetic_layout_yields_the_spill_tail_and_the_outgoing_reserve() {
    let layout = crate::FrameLayout {
        java_locals_hi: 24,
        locals_hi: 40,
        spill_lo: 40,
        spill_hi: 88,
        // A scalar-replaced field inside the spill band must be skipped.
        scalar_lo: 64,
        scalar_hi: 72,
        callee_saved_lo: 96,
        callee_saved_hi: 112,
        reg_spill_lo: 112,
        reg_spill_hi: 128,
        outgoing_lo: 128,
        frame_size: 160,
        ..Default::default()
    };
    let words = dead_words_at_handler_entry(&layout, 40);
    assert_eq!(words, vec![48, 56, 72, 80, 128, 136, 144, 152, 160]);
    for off in &words {
        let region = layout.region_name(*off);
        assert!(
            region == "operand-spill" || *off >= layout.outgoing_lo,
            "offset {off} is in {region}"
        );
    }
}

#[test]
fn no_published_outgoing_boundary_means_no_outgoing_word() {
    let layout = crate::FrameLayout {
        java_locals_hi: 16,
        locals_hi: 24,
        spill_lo: 24,
        spill_hi: 40,
        frame_size: 96,
        ..Default::default()
    };
    assert_eq!(dead_words_at_handler_entry(&layout, 24), vec![32]);
    // No spill band and no boundary: nothing at all.
    assert!(dead_words_at_handler_entry(&crate::FrameLayout::default(), 0).is_empty());
}

/// Against a real compiler's published layout: none of the words is a
/// register image, a deopt image or a local.
#[test]
fn a_real_frame_layout_never_yields_an_image_or_a_local() {
    let c = super::super::emit::tests::test_compiler();
    let layout = c.frame_layout();
    for off in dead_words_at_handler_entry(&layout, c.base_spill_offset) {
        assert!(
            !layout.is_register_image(off),
            "offset {off} is a register image"
        );
        assert!(
            !layout.is_deopt_saved_gpr_image(off),
            "offset {off} is the saved GPRs"
        );
        assert!(
            !layout.is_deopt_saved_xmm_image(off),
            "offset {off} is the saved XMMs"
        );
        assert!(
            off > c.base_spill_offset,
            "offset {off} is at or below slot 0"
        );
        assert!(
            off <= layout.frame_size,
            "offset {off} is outside the frame"
        );
    }
}
