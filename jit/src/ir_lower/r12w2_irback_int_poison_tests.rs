use super::{poison_int_high_bytes, CALL_ARG_REGS, R11};
use crate::ExecutableBuffer;

const RAX: u8 = 0;

/// `MOV dst, src` (REX.W 89 /r).
fn mov64(dst: u8, src: u8) -> [u8; 3] {
    let rex = 0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8);
    [rex, 0x89, 0xC0 | ((src & 7) << 3) | (dst & 7)]
}

fn push(reg: u8) -> Vec<u8> {
    if reg >= 8 {
        vec![0x41, 0x50 | (reg & 7)]
    } else {
        vec![0x50 | reg]
    }
}

fn pop(reg: u8) -> Vec<u8> {
    if reg >= 8 {
        vec![0x41, 0x58 | (reg & 7)]
    } else {
        vec![0x58 | reg]
    }
}

/// `extern "C" fn(u64) -> u64` running `body`, finalized.
fn build(body: &[u8]) -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(body);
    buf.emit(&[0xC3]); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run(buf: &ExecutableBuffer, arg: u64) -> u64 {
    // SAFETY: `buf` was finalized by `build` and holds a complete
    // `extern "C" fn(u64) -> u64` that saves every callee-saved register
    // it writes.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    f(arg)
}

/// `x`, poisoned in `reg`, read back through RAX.
fn poisoned_in(reg: u8, x: u64) -> u64 {
    let (seq, len) = poison_int_high_bytes(reg).expect("a value register");
    // RAX is the result, so it is the one register not saved around.
    let save = reg != RAX;
    let mut body = Vec::new();
    if save {
        body.extend(push(reg));
    }
    body.extend(mov64(reg, CALL_ARG_REGS[0]));
    body.extend(&seq[..len]);
    if save {
        body.extend(mov64(RAX, reg));
        body.extend(pop(reg));
    }
    run(&build(&body), x)
}

#[test]
fn the_poison_keeps_the_low_half_and_is_neither_extension() {
    let regs: [u8; 7] = [RAX, 3, 6, 7, 12, 13, 15];
    let values: [i32; 5] = [0, 7, -7, i32::MIN, i32::MAX];
    for &reg in &regs {
        for &v in &values {
            for x in [v as u32 as u64, v as i64 as u64] {
                let r = poisoned_in(reg, x);
                assert_eq!(
                    r as u32, v as u32,
                    "reg {reg}: the low half moved for {x:#x}"
                );
                let hi = (r >> 32) as u32;
                assert!(
                    hi != 0 && hi != u32::MAX,
                    "reg {reg}: {x:#x} came back as an extension ({r:#x})"
                );
            }
        }
    }
}

#[test]
fn the_poison_leaves_the_flags_and_r11_alone() {
    let (seq, len) = poison_int_high_bytes(RAX).expect("RAX");
    // MOV RAX, arg0; TEST EAX, EAX; <poison>; SETZ CL; MOVZX EAX, CL
    let mut flags = Vec::new();
    flags.extend(mov64(RAX, CALL_ARG_REGS[0]));
    flags.extend([0x85, 0xC0]);
    flags.extend(&seq[..len]);
    flags.extend([0x0F, 0x94, 0xC1, 0x0F, 0xB6, 0xC1]);
    let f = build(&flags);
    assert_eq!(run(&f, 0), 1);
    assert_eq!(run(&f, 5), 0);
    // MOV R11, arg0; <poison RAX>; MOV RAX, R11
    let mut keep = Vec::new();
    keep.extend(mov64(R11, CALL_ARG_REGS[0]));
    keep.extend(&seq[..len]);
    keep.extend(mov64(RAX, R11));
    let k = build(&keep);
    assert_eq!(run(&k, 0x1234_5678_9ABC_DEF0), 0x1234_5678_9ABC_DEF0);
}

#[test]
fn the_poison_declines_rsp_and_its_own_addend() {
    assert!(poison_int_high_bytes(4).is_none());
    assert!(poison_int_high_bytes(R11).is_none());
    assert!(poison_int_high_bytes(16).is_none());
}
