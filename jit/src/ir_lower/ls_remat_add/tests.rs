use super::*;
use crate::ExecutableBuffer;

const RAX: u8 = 0;
const RBX: u8 = 3;
const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;

fn bytes(dst: u8, base: u8, form: RematAddForm, wide: bool) -> Vec<u8> {
    let (b, n) = remat_add_bytes(dst, base, form, wide);
    b[..n].to_vec()
}

// ── Encoding ────────────────────────────────────────────────────────

#[test]
fn j27_remat_add_encodes_the_int_lea_as_32_bit() {
    // LEA EBX, [R12 + 1]: REX.B only (no W), SIB for the R12 base.
    assert_eq!(bytes(RBX, R12, RematAddForm::Lea(1), false), [0x41, 0x8D, 0x5C, 0x24, 0x01]);
    // LEA EBX, [R13 + 0]: an R13 base always carries a displacement.
    assert_eq!(bytes(RBX, R13, RematAddForm::Lea(0), false), [0x41, 0x8D, 0x5D, 0x00]);
}

#[test]
fn j27_remat_add_encodes_the_long_lea_as_64_bit() {
    // LEA R14, [RBX - 200]: REX.W + REX.R, disp32.
    assert_eq!(
        bytes(R14, RBX, RematAddForm::Lea(-200), true),
        [0x4C, 0x8D, 0xB3, 0x38, 0xFF, 0xFF, 0xFF]
    );
}

#[test]
fn j27_remat_add_encodes_mov_add_for_a_wide_addend() {
    // MOV RBX, 0x1_0000_0000 ; ADD RBX, R12
    assert_eq!(
        bytes(RBX, R12, RematAddForm::MovAdd(1 << 32), true),
        [0x48, 0xBB, 0, 0, 0, 0, 1, 0, 0, 0, 0x4C, 0x01, 0xE3]
    );
}

// ── Form choice ─────────────────────────────────────────────────────

#[test]
fn j27_remat_add_takes_lea_whenever_the_addend_fits_a_disp32() {
    assert_eq!(remat_add_form(7, false, true), Some(RematAddForm::Lea(7)));
    assert_eq!(
        remat_add_form(i64::from(i32::MIN), false, false),
        Some(RematAddForm::Lea(i32::MIN))
    );
    assert_eq!(remat_add_form(-1, true, true), Some(RematAddForm::Lea(-1)));
}

#[test]
fn j27_remat_add_refuses_add_while_flags_are_live() {
    assert_eq!(remat_add_form(1 << 32, true, false), Some(RematAddForm::MovAdd(1 << 32)));
    assert_eq!(remat_add_form(1 << 32, true, true), None, "ADD would clobber live flags");
    assert_eq!(
        remat_add_form(i64::from(i32::MAX) + 1, false, false),
        None,
        "an int never takes the 64-bit form"
    );
}

// ── Executed ────────────────────────────────────────────────────────

fn mov64(dst: u8, src: u8) -> [u8; 3] {
    let rex = 0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8);
    [rex, 0x89, 0xC0 | ((src & 7) << 3) | (dst & 7)]
}

fn run(body: &[u8], arg: u64) -> u64 {
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(body);
    buf.emit(&[0xC3]); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` is finalized and holds a complete
    // `extern "C" fn(u64) -> u64` that restores every callee-saved
    // register it writes (RBX, R12) and calls nothing.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    f(arg)
}

/// `PUSH RBX; PUSH R12; MOV R12, arg0; MOV RBX, -1; <remat RBX from R12>;
/// MOV RAX, RBX; POP R12; POP RBX` -- RBX starts all ones so a 32-bit form
/// that failed to zero-extend shows.
fn remat_of(arg: u64, form: RematAddForm, wide: bool) -> u64 {
    let mut body = vec![0x53, 0x41, 0x54];
    body.extend(mov64(R12, super::super::CALL_ARG_REGS[0]));
    body.extend([0x48, 0xC7, 0xC3, 0xFF, 0xFF, 0xFF, 0xFF]); // MOV RBX, -1
    body.extend(bytes(RBX, R12, form, wide));
    body.extend(mov64(RAX, RBX));
    body.extend([0x41, 0x5C, 0x5B]);
    run(&body, arg)
}

#[test]
fn j27_remat_add_int_wraps_at_max_and_zero_extends() {
    let max = i32::MAX as u32 as u64;
    assert_eq!(remat_of(max, RematAddForm::Lea(1), false), 0x8000_0000);
    // The base's high half is not the int's: a 32-bit LEA ignores it.
    let dirty = 0xA5A5_A5A5_0000_0000 | max;
    assert_eq!(remat_of(dirty, RematAddForm::Lea(1), false), 0x8000_0000);
    // `x - MIN` as `x + MIN`, modulo 2^32.
    assert_eq!(remat_of(5, RematAddForm::Lea(i32::MIN), false), 0x8000_0005);
}

#[test]
fn j27_remat_add_long_wraps_at_max() {
    assert_eq!(
        remat_of(i64::MAX as u64, RematAddForm::Lea(1), true) as i64,
        i64::MIN
    );
    assert_eq!(remat_of(5, RematAddForm::Lea(-6), true) as i64, -1);
    assert_eq!(
        remat_of(5, RematAddForm::MovAdd(1 << 40), true) as i64,
        (1 << 40) + 5
    );
    assert_eq!(
        remat_of(i64::MAX as u64, RematAddForm::MovAdd(i64::MIN), true) as i64,
        -1
    );
}

/// `XOR EAX,EAX; PUSH RBX; PUSH R12; MOV R12, arg0; TEST R12, R12;
/// <remat>; SETZ AL; POP R12; POP RBX`: ZF from the TEST survives a LEA
/// and not an ADD -- the reason the ADD form needs the flags dead.
fn zf_after(arg: u64, form: RematAddForm) -> u64 {
    let mut body = vec![0x31, 0xC0, 0x53, 0x41, 0x54];
    body.extend(mov64(R12, super::super::CALL_ARG_REGS[0]));
    body.extend([0x4D, 0x85, 0xE4]); // TEST R12, R12
    body.extend(bytes(RBX, R12, form, true));
    body.extend([0x0F, 0x94, 0xC0]); // SETZ AL
    body.extend([0x41, 0x5C, 0x5B]);
    run(&body, arg)
}

#[test]
fn j27_remat_add_lea_keeps_the_flags_and_add_does_not() {
    assert_eq!(zf_after(0, RematAddForm::Lea(9)), 1);
    assert_eq!(zf_after(0, RematAddForm::MovAdd(1 << 33)), 0);
}
