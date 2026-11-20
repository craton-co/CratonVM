use super::*;
use cratonvm_types::flags::with_thread_overrides;


fn fib_ref(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fib_ref(n - 1) + fib_ref(n - 2)
    }
}

/// The bytes `size_save_bands` takes out of bands of `xmm` and `gpr`
/// bytes when the body saves `xmm_used` XMM and `gpr_used` GP registers:
/// every unused XMM slot, and the unused GPR bytes rounded down to 16.
fn expected_cut(xmm: i32, gpr: i32, xmm_used: i32, gpr_used: i32) -> i32 {
    (xmm - 16 * xmm_used) + ((gpr - 8 * gpr_used) & !15)
}


/// Off changes nothing. On, the one saved register moves into the band's
/// first slot, the unused slots leave the frame, and only what lies deeper
/// than the bands moves (`frame_size`, the image base, the staging top);
/// the spill cap stays, the image still begins where the bands end and
/// abuts the staging, a second call does nothing, and the sized deopt
/// image composes with it.
#[test]
fn r16w17_sized_bands_pack_the_saved_register_and_move_only_what_lies_below() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    for &reg in IR_LOWER_LS_GPRS {
        let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
        let (frame, base, cap, stage_top, xmm, gpr) = (
            lo.frame_size,
            lo.deopt_regs_base,
            lo.spill_cap_off,
            lo.args_stage_top_off,
            lo.saved_xmm_bytes,
            lo.saved_gpr_bytes,
        );
        assert_eq!(
            gpr,
            ir_saved_gpr_bytes(),
            "fixture: the whole file reserved"
        );
        assert!(base > 0, "fixture: a deopt image");
        let unsized_regs: Vec<u8> = lo.saved_gpr_regs().map(|(r, _)| r).collect();
        assert_eq!(lo.size_save_bands(false), 0, "r{reg}: off");
        assert_eq!(
            (lo.frame_size, lo.deopt_regs_base, lo.args_stage_top_off),
            (frame, base, stage_top),
            "r{reg}: off changes nothing"
        );
        let used = i32::from(ir_gp_file().contains(&reg));
        let want = expected_cut(xmm, gpr, 0, used);
        let cut = lo.size_save_bands(true);
        assert_eq!(cut, want, "r{reg}");
        assert!(cut > 0 && cut % 16 == 0, "r{reg}: {cut}");
        assert_eq!(lo.frame_size, frame - cut, "r{reg}");
        assert_eq!(lo.frame_size % 16, frame % 16, "r{reg}: alignment kept");
        assert_eq!(lo.args_stage_top_off, stage_top - cut, "r{reg}");
        assert_eq!(lo.deopt_regs_base, base - cut, "r{reg}");
        assert_eq!(lo.spill_cap_off, cap, "r{reg}: the spill cap stays");
        assert_eq!(lo.saved_xmm_bytes, 0, "r{reg}: no XMM register saved");
        assert_eq!(lo.saved_gpr_bytes, gpr - (want - xmm), "r{reg}");
        let slots: Vec<(u8, i32)> = lo.saved_gpr_regs().collect();
        let regs: Vec<u8> = slots.iter().map(|&(r, _)| r).collect();
        assert_eq!(regs, unsized_regs, "r{reg}: the same registers are saved");
        if used == 1 {
            assert_eq!(slots, vec![(reg, cap + 8)], "r{reg}: the band's first slot");
        }
        // Cast: a small argument count.
        let stage = lo.args_stage_words as i32 * 8;
        let bands_end = |lo: &Lowerer<'static>| {
            lo.spill_cap_off + lo.saved_xmm_bytes + lo.saved_gpr_bytes + lo.deopt_regs_bytes
        };
        assert_eq!(lo.deopt_regs_base, bands_end(&lo), "r{reg}");
        assert_eq!(lo.deopt_regs_base, lo.args_stage_top_off - stage, "r{reg}");
        assert_eq!(lo.size_save_bands(true), 0, "r{reg}: idempotent");
        assert_eq!(lo.frame_size, frame - cut);
        lo.size_deopt_image(true);
        assert!(lo.deopt_regs_bytes < DEOPT_IMAGE_FULL_BYTES, "r{reg}");
        assert_eq!(lo.deopt_regs_base, bands_end(&lo), "r{reg}: composed");
        assert_eq!(
            lo.deopt_regs_base,
            lo.args_stage_top_off - stage,
            "r{reg}: composed"
        );
    }
}

/// EXECUTED: a deopt through a frame with packed bands, full and sized
/// image, reads the register's value back out of the image, finds the
/// word below `gpr[0]` untouched, and the stub's teardown restores the
/// caller's register from its packed slot (a wrong slot hands the Rust
/// caller a corrupt callee-saved register).
#[test]
fn r16w17_a_deopt_through_a_packed_frame_reads_the_register_and_restores_it() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    for &reg in IR_LOWER_LS_GPRS {
        if !ir_gp_file().contains(&reg) {
            continue;
        }
        for image_sized in [false, true] {
            let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
            assert!(lo.size_save_bands(true) > 0, "r{reg}");
            lo.size_deopt_image(image_sized);
            let value = 0x5EED_0F0F_1234_5678i64 ^ i64::from(reg);
            let locals = super::r16w11_frames2_tests::deopt_through(lo, Some((reg, value)));
            assert_eq!(
                locals,
                vec![
                    FrameValue::Long(super::r16w11_frames2_tests::POISON),
                    FrameValue::Long(value)
                ],
                "r{reg} image_sized={image_sized}"
            );
        }
    }
}



/// The fake thread the watermark capture reads: word 1 is the shadow
/// `top`. A promoted constant, so its address is stable and it needs no
/// `static`.
const FAKE_THREAD: [u64; 4] = [0, 0x5EED_7A1E_0000_0001, 0, 0];

extern "C" fn fake_current_thread() -> u64 {
    // Cast: the promoted constant's address, handed out as the thread.
    (&FAKE_THREAD as *const [u64; 4]) as u64
}


