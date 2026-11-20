use super::*;
use crate::deopt::take_last_deopt;
use cratonvm_types::flags::with_thread_overrides;

/// Distinctive in both halves (a `RegisterLong` read truncated to 32 bits,
/// or taken from the wrong word, cannot produce it by accident).
const SENTINEL: i64 = 0x5EED_1234_0BAD_C0DEu64 as i64;
pub(super) const POISON: i64 = 0x0DDB_A11F_00D5_7A1Eu64 as i64;

fn saved(regs: &[u8]) -> [bool; 16] {
    let mut set = [false; 16];
    for &r in regs {
        set[r as usize] = true;
    }
    set
}

fn fib_ref(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fib_ref(n - 1) + fib_ref(n - 2)
    }
}

/// `gpr[0..=max(5, k)]`, rounded up to 16 bytes, nothing for an empty
/// plan, and never the XMM half.
#[test]
fn r16w11_the_sized_image_spans_gpr0_to_the_highest_register_and_the_rbp_image() {
    assert_eq!(
        sized_deopt_image_bytes(&saved(&[])),
        0,
        "no register, no image"
    );
    assert_eq!(sized_deopt_image_bytes(&saved(&[3])), 48, "gpr[0..=5]");
    assert_eq!(
        sized_deopt_image_bytes(&saved(&[6])),
        64,
        "7 words, rounded"
    );
    assert_eq!(sized_deopt_image_bytes(&saved(&[3, 7])), 64);
    assert_eq!(
        sized_deopt_image_bytes(&saved(&[3, 12])),
        112,
        "13 words, rounded"
    );
    assert_eq!(sized_deopt_image_bytes(&saved(&[3, 12, 13, 14, 15])), 128);
    for &reg in IR_LOWER_LS_GPRS {
        let bytes = sized_deopt_image_bytes(&saved(&[reg]));
        assert_eq!(bytes % 16, 0, "r{reg}: 16-aligned");
        assert!(bytes <= 128, "r{reg}: never the XMM half");
        assert!(i32::from(reg) * 8 < bytes, "r{reg}: the register's word");
        assert!(5 * 8 < bytes, "r{reg}: the RBP image");
    }
}

/// Round 25 item 1b: the caller-saved file is never deopt-nameable, so
/// the sized image is cut for the rest of the set only. A body whose only
/// registers are R8-R11 reserved `gpr[0..=11]` (96 bytes) before.
#[test]
fn r25_the_sized_image_leaves_out_the_caller_saved_file() {
    for &r in IR_LS_CALLER_SAVED_GPRS {
        assert_eq!(
            sized_deopt_image_bytes(&saved(&[r])),
            ((i32::from(r) + 1) * 8 + 15) & !15,
            "r{r}: the old reservation, gpr[0..=r] (96 bytes for R11)"
        );
        assert_eq!(
            sized_deopt_image_bytes(&deopt_image_named_gprs(&saved(&[r]))),
            0,
            "r{r}: alone, no image"
        );
        assert_eq!(
            sized_deopt_image_bytes(&deopt_image_named_gprs(&saved(&[3, r]))),
            48,
            "r{r} with RBX: gpr[0..=5]"
        );
        assert_eq!(
            deopt_image_named_gprs(&saved(&[3, 12, r]))
                .iter()
                .enumerate()
                .filter(|&(_, &on)| on)
                .map(|(i, _)| i)
                .collect::<Vec<_>>(),
            vec![3, 12],
            "r{r}: the callee-saved registers stay"
        );
    }
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    for &r in IR_LS_CALLER_SAVED_GPRS {
        let mut lo = super::tests::lowerer_with_resident_gpr(16384, r);
        let frame = lo.frame_size;
        lo.size_deopt_image(true);
        assert_eq!(lo.deopt_regs_bytes, 0, "r{r}: nothing nameable, no image");
        assert_eq!(lo.frame_size, frame - DEOPT_IMAGE_FULL_BYTES, "r{r}");
        assert!(!lo.deopt_image_holds_gpr(r), "r{r}: never in the image");
    }
}

/// With the switch off (the default) sizing changes nothing; with it on,
/// only the image and what lies below it move: `frame_size` and the
/// staging top drop by the cut, every shallower offset stays, the image
/// still abuts the staging region, and the published ranges name exactly
/// the reserved words (`hi == base + 8`, no XMM half).
#[test]
fn r16w11_sizing_moves_only_the_image_and_the_staging_below_it() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    assert!(
        !with_thread_overrides(&[], ir_deopt_image_sized_enabled),
        "default OFF"
    );
    for &reg in IR_LOWER_LS_GPRS {
        let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
        let (frame, base, spill_cap, stage_top) = (
            lo.frame_size,
            lo.deopt_regs_base,
            lo.spill_cap_off,
            lo.args_stage_top_off,
        );
        assert_eq!(lo.deopt_regs_bytes, DEOPT_IMAGE_FULL_BYTES);
        lo.size_deopt_image(false);
        assert_eq!(
            (lo.frame_size, lo.deopt_regs_base, lo.deopt_regs_bytes),
            (frame, base, DEOPT_IMAGE_FULL_BYTES),
            "r{reg}: off changes nothing"
        );
        lo.size_deopt_image(true);
        let want = sized_deopt_image_bytes(&saved(&[reg]));
        let cut = DEOPT_IMAGE_FULL_BYTES - want;
        assert_eq!(lo.deopt_regs_bytes, want, "r{reg}");
        assert_eq!(
            lo.frame_size,
            frame - cut,
            "r{reg}: the frame shrinks by the cut"
        );
        assert_eq!(lo.frame_size % 16, frame % 16, "r{reg}: alignment kept");
        assert_eq!(lo.args_stage_top_off, stage_top - cut, "r{reg}");
        assert_eq!(
            lo.spill_cap_off, spill_cap,
            "r{reg}: shallower offsets stay"
        );
        assert_eq!(
            lo.deopt_regs_base,
            lo.spill_cap_off + lo.saved_xmm_bytes + lo.saved_gpr_bytes + want,
            "r{reg}: the image ends where the save bands begin"
        );
        // Cast: a small argument count.
        let stage = lo.args_stage_words as i32 * 8;
        assert_eq!(
            lo.deopt_regs_base,
            lo.args_stage_top_off - stage,
            "r{reg}: the image abuts the staging region"
        );
        assert!(lo.deopt_image_holds_gpr(reg), "r{reg}: held");
        assert!(lo.deopt_image_holds_gpr(5), "r{reg}: the RBP image is held");
        assert_eq!(lo.deopt_xmm_image(), None, "r{reg}: no XMM half");
        let (glo, ghi) = lo.deopt_gpr_image();
        assert_eq!(ghi, lo.deopt_regs_base + 8, "r{reg}: hi is gpr[0] + 8");
        assert_eq!(ghi - glo, want, "r{reg}: exactly the reserved words");
        assert!(
            glo > lo.spill_cap_off + lo.saved_xmm_bytes + lo.saved_gpr_bytes,
            "r{reg}: the published range stays inside the reservation"
        );
        // The collector's RBP-image slot (`deopt_image_rbp_slot`).
        let rbp_slot = ghi - 8 - 5 * 8;
        assert!(rbp_slot >= glo && rbp_slot < ghi, "r{reg}");
        // Sizing twice is idempotent.
        let after = (lo.frame_size, lo.deopt_regs_base, lo.deopt_regs_bytes);
        lo.size_deopt_image(true);
        assert_eq!(
            (lo.frame_size, lo.deopt_regs_base, lo.deopt_regs_bytes),
            after
        );
    }
    // No register in the plan: no image at all, and the frame loses all 256.
    let mut bare = super::tests::lowerer_with_resident_xmm(16384, None);
    let frame = bare.frame_size;
    bare.size_deopt_image(true);
    assert_eq!(bare.deopt_regs_base, 0);
    assert_eq!(bare.deopt_regs_bytes, 0);
    assert_eq!(bare.frame_size, frame - DEOPT_IMAGE_FULL_BYTES);
    assert_eq!(bare.deopt_gpr_image(), (0, 0));
    assert_eq!(bare.deopt_xmm_image(), None);
    assert!(!bare.deopt_image_holds_gpr(3));
}

/// A register outside the sized image is never named by a frame state.
#[test]
fn r16w11_a_register_outside_the_image_is_never_named() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    let reg = *IR_LOWER_LS_GPRS.first().expect("a GP residency file");
    let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
    lo.size_deopt_image(true);
    assert!(lo.deopt_nameable[0], "fixture: node 0 owns its register");
    assert_eq!(
        lo.register_frame_value(0, IrType::Long),
        Some(FrameValue::RegisterLong(reg)),
        "inside the image: named"
    );
    // Shrink the image under the register by hand (never happens: the
    // image holds every register the plan hands out).
    lo.deopt_regs_bytes = (i32::from(reg) * 8) & !15;
    assert!(!lo.deopt_image_holds_gpr(reg));
    assert_eq!(
        lo.register_frame_value(0, IrType::Long),
        None,
        "outside: not named"
    );
}

/// The stub stores the XMM file only into a full image: a sized one has
/// no XMM half, and a store there would land on the save bands.
#[test]
fn r16w11_the_stub_stores_no_xmm_word_into_a_sized_image() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    let movsd = |code: &[u8]| {
        code.windows(4)
            .filter(|w| w[..3] == [0xF2, 0x0F, 0x11] || *w == [0xF2, 0x44, 0x0F, 0x11])
            .count()
    };
    for sized in [false, true] {
        let mut lo = super::tests::lowerer_with_resident_gpr(16384, 3);
        lo.size_deopt_image(sized);
        lo.buf.emit(&[0, 0, 0, 0]);
        lo.deopt_stub_patches.push(0);
        let at = lo.buf.pos();
        lo.emit_deopt_stub();
        let n = movsd(&lo.buf.as_slice()[at..]);
        assert_eq!(n, if sized { 0 } else { 16 }, "sized={sized}");
    }
}

/// Execute a deopt through `lo`'s frame: `reg` holds `value` (and no frame
/// word does), the guard's frame state names `RegisterLong(reg)` and the
/// word just deeper than the image's `gpr[0]`, which this body poisons
/// first. Returns the reconstructed locals. A store outside the reserved
/// words either changes the poisoned word or (a stray XMM half, which in a
/// sized frame reaches the save bands and beyond) the restored registers
/// and the frame record.
pub(super) fn deopt_through(
    mut lo: Lowerer<'static>,
    reg: Option<(u8, i64)>,
) -> Vec<FrameValue> {
    let deep = if lo.deopt_regs_base > 0 {
        lo.deopt_regs_base + 8
    } else {
        lo.args_stage_top_off + 8
    };
    assert!(
        deep < lo.frame_size,
        "the poisoned word is inside the frame"
    );
    let mut locals = vec![FrameValue::StackSlotLong(-deep)];
    if let Some((r, _)) = reg {
        locals.push(FrameValue::RegisterLong(r));
    }
    // LEAK(intentional): the stub bakes this pointer as an immediate.
    let point: &'static DeoptimizationPoint = Box::leak(Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci: 7,
        reason: DeoptReason::TransferToInterpreter,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 7,
            locals,
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(DeoptReason::TransferToInterpreter),
    }));
    // push rbp ; mov rbp, rsp ; sub rsp, frame_size
    lo.buf.emit_byte(0x55);
    lo.buf.emit(&[0x48, 0x89, 0xE5]);
    lo.buf.emit(&[0x48, 0x81, 0xEC]);
    let frame = lo.frame_size;
    lo.buf.emit(&frame.to_le_bytes());
    // What the stub's teardown restores.
    let gpr_saves: Vec<(u8, i32)> = lo.saved_gpr_regs().collect();
    if let Some((r, _)) = reg {
        // The body writes `r`; every register of the residency file is
        // callee-saved, so the teardown must restore it or the caller's
        // value is lost.
        assert!(
            gpr_saves.iter().any(|&(s, _)| s == r),
            "r{r} is not saved by this frame: the body would clobber the caller's register"
        );
    }
    for (r, off) in gpr_saves {
        lo.emit_gpr_frame_move(r, off, true);
    }
    let xmm_saves: Vec<(u8, i32)> = lo.saved_xmm_regs().collect();
    for (r, off) in xmm_saves {
        lo.emit_xmm_frame_move(r, off, true);
    }
    // Cast: fixed test sentinels.
    lo.emit_mov_reg_imm64(RAX, POISON as u64);
    lo.store_abi_reg(RAX, deep);
    if let Some((r, v)) = reg {
        lo.emit_mov_reg_imm64(r, v as u64);
    }
    // Cast: a leaked pointer baked as the guard's argument.
    lo.emit_mov_reg_imm64(DEOPT_ARG0, point as *const DeoptimizationPoint as u64);
    // JMP <stub>, patched by `emit_deopt_stub` exactly as a guard's is.
    lo.buf.emit_byte(0xE9);
    let patch = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    lo.deopt_stub_patches.push(patch);
    lo.emit_deopt_stub();
    assert!(!lo.buf.overflowed(), "the harness body must fit");
    let cm = CompiledMethod::new(lo.buf);
    let _ = take_last_deopt();
    // SAFETY: the body takes no arguments, builds and tears down its own
    // frame (restoring every callee-saved register it wrote), and calls
    // only `ir_deopt_entry`.
    let ret = unsafe { cm.try_call(&[]) }.expect("the harness body runs");
    assert_eq!(ret, i64::MIN, "the stub returns the deopt sentinel");
    take_last_deopt().expect("the entry stashed a frame").locals
}

/// The executed half: a deopt through every shrunk layout this platform's
/// residency file can produce (one per register: 48, 112 and 128 bytes,
/// and 64 for Win64's RSI/RDI under `CRATONVM_JIT_IR_GP_WIDE=1`), and
/// through the full image, reads the register's value back out of the
/// image and finds the word below `gpr[0]` untouched.
///
/// Over `ir_gp_file()`, the file a compile hands out, and not
/// `IR_LOWER_LS_GPRS`: on Win64 the wide file adds RSI/RDI, which the
/// prologue saves (and the stub's teardown restores) only when the wide
/// switch is on. Writing one in this body with the switch off (the
/// default) clobbered the test thread's callee-saved RSI, and the harness
/// died `STATUS_ACCESS_VIOLATION` after the body returned
/// (`gci-i1orch-r16w11-sized-layout-deopt-test-access-violation-on-windows`,
/// fixed 2026-10-16).
#[test]
fn r16w11_a_deopt_through_each_sized_layout_reads_the_spilled_register() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    let mut sizes: Vec<i32> = Vec::new();
    for &reg in ir_gp_file() {
        for sized in [false, true] {
            let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
            lo.size_deopt_image(sized);
            let bytes = lo.deopt_regs_bytes;
            if sized {
                assert!(bytes < DEOPT_IMAGE_FULL_BYTES, "r{reg}: the image shrank");
                sizes.push(bytes);
            }
            let value = SENTINEL ^ i64::from(reg);
            let locals = deopt_through(lo, Some((reg, value)));
            assert_eq!(
                locals,
                vec![FrameValue::Long(POISON), FrameValue::Long(value)],
                "r{reg} sized={sized} ({bytes}-byte image)"
            );
        }
    }
    sizes.sort_unstable();
    sizes.dedup();
    assert!(
        sizes.len() >= 2,
        "more than one shrunk layout exercised: {sizes:?}"
    );
}

/// No register in the plan: the sized frame has no image, the stub hands
/// `ir_deopt_entry` NULL, and a deopt naming only frame words still
/// reconstructs them.
#[test]
fn r16w11_a_deopt_through_a_frame_with_no_image_reads_its_frame_words() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    let mut lo = super::tests::lowerer_with_resident_xmm(16384, None);
    lo.size_deopt_image(true);
    assert_eq!(lo.deopt_regs_base, 0);
    let locals = deopt_through(lo, None);
    assert_eq!(locals, vec![FrameValue::Long(POISON)]);
}


