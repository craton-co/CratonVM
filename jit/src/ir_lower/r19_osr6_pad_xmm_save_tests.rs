use super::{
    emit_cell_writeback, emit_pad_xmm_restore, emit_pad_xmm_save, frame_named_xmms,
    pad_xmm_save_bytes, CellWriteBack, CellWriteBackSrc, CALL_ARG_REGS,
};
use crate::deopt::{FrameState, FrameValue};
use crate::ExecutableBuffer;

/// A field helper that clobbers XMM0..XMM5 (caller-saved on both ABIs),
/// then stores like the real one. No `static`.
extern "C" fn clobbering_putfield(obj: i64, field_index: i64, val: i64) {
    // SAFETY: the test passes a live `[i64; 4]` and an index < 4. The asm
    // writes only registers it declares clobbered.
    unsafe {
        std::arch::asm!(
            "xorps xmm0, xmm0", "xorps xmm1, xmm1", "xorps xmm2, xmm2",
            "xorps xmm3, xmm3", "xorps xmm4, xmm4", "xorps xmm5, xmm5",
            out("xmm0") _, out("xmm1") _, out("xmm2") _,
            out("xmm3") _, out("xmm4") _, out("xmm5") _,
        );
        *(obj as *mut i64).add(field_index as usize) = val;
    }
}

/// `extern "C" fn(vals, frame)`: load XMM r from `vals[r]` for each `r` in
/// `xmms`, run one write-back between the save and the restore, store the
/// registers back into `vals`. RBP = `&frame[8]`, as the pad addresses it.
fn run(xmms: &[u8], saved: &[u8], vals: &mut [u64; 6], frame: &mut [i64; 16]) {
    let mut buf = ExecutableBuffer::new(2048).expect("executable buffer");
    let (a0, a1) = (CALL_ARG_REGS[0], CALL_ARG_REGS[1]);
    buf.emit(&[0x55]); // PUSH RBP
    buf.emit(&[0x53]); // PUSH RBX
                       // MOV RBX, arg0 (vals); MOV RBP, arg1 (frame).
    buf.emit(&[
        0x48 | (u8::from(a0 >= 8) << 2),
        0x89,
        0xC0 | ((a0 & 7) << 3) | 3,
    ]);
    buf.emit(&[
        0x48 | (u8::from(a1 >= 8) << 2),
        0x89,
        0xC0 | ((a1 & 7) << 3) | 5,
    ]);
    buf.emit(&[0x48, 0x83, 0xEC, 40]); // SUB RSP, 40 (aligned + home space)
    for &x in xmms {
        // MOVSD xmm, [RBX + 8x]
        buf.emit(&[0xF2, 0x0F, 0x10, 0x43 | ((x & 7) << 3), 8 * x]);
    }
    emit_pad_xmm_save(&mut buf, saved);
    let wb = CellWriteBack {
        base_off: -48,
        field_index: 1,
        long: true,
        value: CellWriteBackSrc::Imm(42),
        init: CellWriteBackSrc::Imm(0),
        // Cast: the stand-in helper's address.
        helper: clobbering_putfield as *const () as u64,
        static_cell: None,
    };
    emit_cell_writeback(&mut buf, &wb);
    emit_pad_xmm_restore(&mut buf, saved);
    for &x in xmms {
        // MOVSD [RBX + 8x], xmm
        buf.emit(&[0xF2, 0x0F, 0x11, 0x43 | ((x & 7) << 3), 8 * x]);
    }
    buf.emit(&[0x48, 0x83, 0xC4, 40]); // ADD RSP, 40
    buf.emit(&[0x5B, 0x5D, 0xC3]); // POP RBX; POP RBP; RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` holds a complete `extern "C" fn(u64, u64)` restoring
    // every callee-saved register it writes; it touches only XMM0..XMM5.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64, u64)>(buf.as_ptr()) };
    // Cast: the two buffers' addresses.
    f(vals.as_mut_ptr() as u64, frame[8..].as_mut_ptr() as u64);
}

#[test]
fn the_named_xmm_registers_survive_the_helper_call() {
    let mut cells = [0i64; 4];
    let mut frame = [0i64; 16];
    // `[RBP - 48]` = frame[2]: the object's reference frame word.
    // Cast: the object's address.
    frame[2] = cells.as_mut_ptr() as i64;
    let mut vals = [
        1.5f64.to_bits(),
        2.5f64.to_bits(),
        0,
        3.25f64.to_bits(),
        0,
        7.0f64.to_bits(),
    ];
    let want = vals;
    run(&[0, 1, 3, 5], &[0, 1, 3, 5], &mut vals, &mut frame);
    assert_eq!(cells[1], 42, "the write-back ran");
    assert_eq!(vals, want, "every saved register is restored");
    // The control: without the save the helper's clobber shows.
    let mut vals2 = want;
    run(&[0, 1, 3, 5], &[], &mut vals2, &mut frame);
    assert_eq!(vals2, [0u64; 6], "unsaved, the helper zeroed them");
}

#[test]
fn the_save_keeps_the_calls_aligned_and_encodes_high_registers() {
    assert_eq!(pad_xmm_save_bytes(1) % 16, 0);
    assert_eq!(pad_xmm_save_bytes(3) % 16, 0);
    assert_eq!(pad_xmm_save_bytes(16), 32 + 128);
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    emit_pad_xmm_save(&mut buf, &[13]);
    // SAFETY: the buffer's first `pos()` bytes were just written.
    let code = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.pos()) };
    // SUB RSP, 48; MOVSD [RSP + 32], XMM13 (F2 REX.R 0F 11 /r, SIB RSP).
    assert_eq!(
        code,
        &[0x48, 0x81, 0xEC, 48, 0, 0, 0, 0xF2, 0x44, 0x0F, 0x11, 0xAC, 0x24, 32, 0, 0, 0]
    );
    let mut none = ExecutableBuffer::new(64).expect("executable buffer");
    emit_pad_xmm_save(&mut none, &[]);
    emit_pad_xmm_restore(&mut none, &[]);
    assert_eq!(none.pos(), 0, "nothing named: no code");
}

#[test]
fn the_named_set_covers_locals_stack_virtual_fields_and_callers() {
    let frame = |locals: Vec<FrameValue>, stack: Vec<FrameValue>| FrameState {
        class_id: 0,
        method_key: String::new(),
        bci: 0,
        locals,
        stack,
        monitors: Vec::new(),
        caller: None,
    };
    let caller = frame(vec![FrameValue::XmmDouble(9)], Vec::new());
    let mut fs = frame(
        vec![FrameValue::XmmFloat(3), FrameValue::Register(12)],
        vec![FrameValue::XmmDouble(3)],
    );
    fs.caller = Some(Box::new(caller));
    assert_eq!(frame_named_xmms(&fs), vec![3, 9]);
}
