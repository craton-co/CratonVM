use super::*;

/// The frame of `only_the_callee_saved_gpr_image_is_rewritten`: GPR image
/// 48..80, XMM image 80..112, blind GPR spill image 112..144.
fn layout() -> cratonvm_jit::FrameLayout {
    cratonvm_jit::FrameLayout {
        java_locals_hi: 16,
        spill_lo: 16,
        spill_hi: 48,
        callee_saved_lo: 48,
        callee_saved_hi: 80,
        xmm_saved_lo: 80,
        xmm_saved_hi: 112,
        reg_spill_lo: 112,
        reg_spill_hi: 144,
        frame_size: 176,
        ..Default::default()
    }
}

/// gci i8/nt: a blind-spill image word is movable only when the register
/// it copies is refreshed after the safepoint (the shadow reload); with
/// shadow emission off it must pin. The callee-saved GPR image is resumed
/// from by the epilogue, so it stays movable either way, and the reload
/// question is not even asked for it.
#[test]
fn gci_i8nt_spill_image_word_is_movable_only_with_the_shadow_reload() {
    let layout = layout();
    for off in [112, 120, 136] {
        assert!(layout.spill_image_index(off).is_some(), "off={off} is spill image");
        assert!(
            register_image_word_publishes_movable(off, &layout, || true),
            "off={off}: the shadow reload refreshes the register, so movable"
        );
        assert!(
            !register_image_word_publishes_movable(off, &layout, || false),
            "off={off}: nothing reloads the register, so the word must pin"
        );
    }
    for off in [48, 56, 72] {
        assert!(layout.spill_image_index(off).is_none());
        assert!(
            register_image_word_publishes_movable(off, &layout, || {
                panic!("the reload is asked only for a spill-image word")
            }),
            "off={off}: the callee-saved GPR image is resumed from, so movable"
        );
    }
}

// The switch (default on since 1eb8ed7865) pins spill-image words even with shadow emission on;
// `=0` leaves the default answer, which is shadow emission's.
