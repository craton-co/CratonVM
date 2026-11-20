use super::{emit_cell_writeback, CellWriteBack, CellWriteBackSrc, CALL_ARG_REGS};
use crate::ExecutableBuffer;

/// The field helper's stand-in: `obj` is an `i64` array, `field_index`
/// its element. No `static` (`process_global_statics_ratchet`).
extern "C" fn fake_putfield(obj: i64, field_index: i64, val: i64) {
    // SAFETY: the tests below pass a live `[i64; 4]` and an index < 4.
    unsafe { *(obj as *mut i64).add(field_index as usize) = val }
}

/// `extern "C" fn(frame_ptr)`: RBP = `frame_ptr`, RBX = `0x77`, then the
/// write-backs, at a 16-aligned RSP with 40 bytes of home space.
fn run(wbs: &[CellWriteBack], frame: &mut [i64; 16]) {
    let mut buf = ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&[0x55]); // PUSH RBP
    buf.emit(&[0x53]); // PUSH RBX
    let a0 = CALL_ARG_REGS[0];
    // MOV RBP, arg0
    buf.emit(&[
        0x48 | (u8::from(a0 >= 8) << 2),
        0x89,
        0xC0 | ((a0 & 7) << 3) | 5,
    ]);
    buf.emit(&[0x48, 0xC7, 0xC3, 0x77, 0, 0, 0]); // MOV RBX, 0x77
    buf.emit(&[0x48, 0x83, 0xEC, 40]); // SUB RSP, 40
    for wb in wbs {
        emit_cell_writeback(&mut buf, wb);
    }
    buf.emit(&[0x48, 0x83, 0xC4, 40]); // ADD RSP, 40
    buf.emit(&[0x5B, 0x5D, 0xC3]); // POP RBX; POP RBP; RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` holds a complete `extern "C" fn(u64)` that restores
    // every callee-saved register it writes (RBX, RBP).
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64)>(buf.as_ptr()) };
    // Cast: the frame pointer the sequence addresses `[RBP + off]` from.
    f(frame[8..].as_mut_ptr() as u64);
}

/// `[RBP + off]` for `frame[i]` when RBP = `&frame[8]`.
fn off(i: usize) -> i32 {
    // Cast: a 0..16 index into a frame displacement.
    (i as i32 - 8) * 8
}

fn wb(
    long: bool,
    field_index: i64,
    value: CellWriteBackSrc,
    init: CellWriteBackSrc,
) -> CellWriteBack {
    CellWriteBack {
        base_off: off(2),
        field_index,
        long,
        value,
        init,
        // Cast: the stand-in helper's address.
        helper: fake_putfield as *const () as u64,
        static_cell: None,
    }
}

fn with_object(cells: &mut [i64; 4], frame: &mut [i64; 16]) {
    // Cast: the object's address in its reference frame word.
    frame[2] = cells.as_mut_ptr() as i64;
}

#[test]
fn a_changed_int_cell_is_written_back_and_an_unchanged_one_is_not() {
    let mut cells = [-1i64; 4];
    let mut frame = [0i64; 16];
    with_object(&mut cells, &mut frame);
    frame[3] = 7; // the value on the back edge
    frame[4] = 5; // the value read on entry
    frame[5] = 5;
    run(
        &[
            wb(
                false,
                1,
                CellWriteBackSrc::Frame(off(3)),
                CellWriteBackSrc::Frame(off(4)),
            ),
            wb(
                false,
                2,
                CellWriteBackSrc::Frame(off(5)),
                CellWriteBackSrc::Frame(off(4)),
            ),
        ],
        &mut frame,
    );
    assert_eq!(cells[1], 7, "the store the loop made reaches memory");
    assert_eq!(
        cells[2], -1,
        "a cell back at its entry value is not written"
    );
    assert_eq!((cells[0], cells[3]), (-1, -1), "nothing else is touched");
}

/// An `int` compares its low half only (a home word's high half is not
/// the value); a `long` compares and stores all 64 bits.
#[test]
fn int_compares_32_bits_and_long_64() {
    let mut cells = [-1i64; 4];
    let mut frame = [0i64; 16];
    with_object(&mut cells, &mut frame);
    frame[3] = 0x1_0000_0005;
    frame[4] = 5;
    run(
        &[
            wb(
                false,
                0,
                CellWriteBackSrc::Frame(off(3)),
                CellWriteBackSrc::Frame(off(4)),
            ),
            wb(
                true,
                1,
                CellWriteBackSrc::Frame(off(3)),
                CellWriteBackSrc::Frame(off(4)),
            ),
        ],
        &mut frame,
    );
    assert_eq!(cells[0], -1, "equal as ints: no write");
    assert_eq!(cells[1], 0x1_0000_0005, "different as longs: all 64 bits");
}

/// Register and immediate operands; a residency register survives the
/// first helper call for the second write-back.
#[test]
fn register_and_immediate_operands() {
    let mut cells = [-1i64; 4];
    let mut frame = [0i64; 16];
    with_object(&mut cells, &mut frame);
    run(
        &[
            wb(false, 0, CellWriteBackSrc::Reg(3), CellWriteBackSrc::Imm(0)),
            wb(true, 3, CellWriteBackSrc::Imm(-9), CellWriteBackSrc::Reg(3)),
            wb(
                false,
                1,
                CellWriteBackSrc::Reg(3),
                CellWriteBackSrc::Imm(0x77),
            ),
        ],
        &mut frame,
    );
    assert_eq!(cells[0], 0x77, "RBX, read before the first call");
    assert_eq!(cells[3], -9, "an immediate value, RBX read after a call");
    assert_eq!(cells[1], -1, "RBX equals its entry value: no write");
}

/// IROPS2-4 (lane osr4): the OSR entry stub's re-read of a hoisted
/// `float` static ZERO-extends its 32-bit payload (as
/// `emit_inline_getstatic` does), and a `double` / reference one reads all
/// 64 bits. Run against a fake statics cell.
#[test]
fn r19_osr4_the_wide_static_remat_reads_with_the_getstatic_widths() {
    use super::OsrRematInsn;
    // A statics block and the cell holding its address (both leaked: no
    // `static`, `process_global_statics_ratchet`).
    let block: &'static mut [u64; 8] = Box::leak(Box::new([0u64; 8]));
    block[1] = u64::from((-1.5f32).to_bits()) | 0xDEAD_BEEF_0000_0000;
    block[2] = (-2.25f64).to_bits();
    let cell: &'static mut u64 = Box::leak(Box::new(block.as_ptr() as u64)); // Cast: block address
    let base_cell = cell as *const u64 as u64; // Cast: cell address
    let run = |insn: OsrRematInsn| -> u64 {
        let mut lo = super::tests::lowerer_with_resident_xmm(4096, None);
        let start = lo.buf.pos();
        lo.emit_osr_remat_insn(insn);
        let code = lo.buf.as_slice()[start..].to_vec();
        let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
        buf.emit(&code);
        buf.emit(&[0xC3]); // RET
        buf.finalize();
        // SAFETY: the sequence writes RAX only and reads the leaked cell
        // and block above.
        let f =
            unsafe { std::mem::transmute::<*const u8, extern "C" fn() -> u64>(buf.as_ptr()) };
        f()
    };
    let f32_bits = run(OsrRematInsn::StaticLoadF32 { base_cell, disp: 8 });
    assert_eq!(
        f32_bits,
        u64::from((-1.5f32).to_bits()),
        "a negative float's word is zero-extended, not sign-extended or read whole"
    );
    let d_bits = run(OsrRematInsn::StaticLoad {
        base_cell,
        disp: 16,
        wide: true,
    });
    assert_eq!(d_bits, (-2.25f64).to_bits());
}


