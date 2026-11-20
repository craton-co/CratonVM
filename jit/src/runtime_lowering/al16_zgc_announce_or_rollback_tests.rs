use super::*;

const R8: u8 = 8;
const R9: u8 = 9;
const OUT_OFF: i32 = 8;
const SENTINELS: [u64; 4] = [
    0x1111_2222_3333_4444,
    0x5555_6666_7777_8888,
    0x9999_AAAA_BBBB_CCCC,
    0xDDDD_EEEE_FFFF_0123,
];

/// `extern "C" fn(thread, out, obj) -> obj | 0`: R10 = thread, R11 = obj,
/// RCX/RDX/R8/R9 = the sentinels, then the sequence; `out` receives the
/// four registers afterwards on both arms.
fn build() -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(1024)?;
    // PUSH RBP; MOV RBP, RSP; SUB RSP, 16.
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x10]);
    emit_store_frame(&mut buf, OUT_OFF, ENTRY_ABI_REGS[1]);
    emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
    emit_mov_reg(&mut buf, R11, ENTRY_ABI_REGS[2]);
    for (reg, v) in [RCX, RDX, R8, R9].into_iter().zip(SENTINELS) {
        emit_mov_imm64(&mut buf, reg, v);
    }
    let rolled_back = emit_zgc_announce_or_rollback(&mut buf, 0, 8);
    let announced = emit_jmp(&mut buf);
    for p in rolled_back {
        patch_rel32_to_here(&mut buf, p);
    }
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    patch_rel32_to_here(&mut buf, announced);
    emit_load_frame(&mut buf, R10, OUT_OFF);
    // MOV [R10+0], RCX; [R10+8], RDX; [R10+16], R8; [R10+24], R9.
    buf.emit(&[0x49, 0x89, 0x4A, 0x00, 0x49, 0x89, 0x52, 0x08]);
    buf.emit(&[0x4D, 0x89, 0x42, 0x10, 0x4D, 0x89, 0x4A, 0x18]);
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    if buf.overflowed() {
        return None;
    }
    buf.finalize();
    Some(buf)
}

#[repr(C, align(512))]
struct Chunk([u64; 512]);

/// `(returned obj, cursor after, bitmap word 0, registers after)` for an
/// object 64 bytes into a 4 KiB, 512-aligned TLAB already committed past
/// it by 16 bytes. `table` = `None` for an unpublished table, else
/// `(blocked, epoch)` against the TLAB's owned epoch 7.
fn run(table: Option<(u64, u64)>) -> Option<(u64, u64, u64, [u64; 4])> {
    let buf = build()?;
    let chunk = Box::new(Chunk([0; 512]));
    let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
    let obj = base + 64;
    let mut bitmap = [0u64; 8];
    // words (the bitmap), base, span, blocked, epoch.
    // words, base, span, blocked, epoch, -, short_blocked (zr z9/bo; the lean store gates on it), bias.
    let mut tab = [bitmap.as_mut_ptr() as u64, base, 4096, 0, 0, 0, 0, 0];
    let announce = match table {
        None => 0,
        Some((blocked, epoch)) => {
            tab[3] = blocked;
            tab[6] = blocked;
            tab[4] = epoch;
            tab.as_ptr() as u64
        }
    };
    // cursor, end, start, owned epoch, announce table, owned-word window
    // (zr z11/ia: words [0, 8) of the 4 KiB chunk at the bitmap base).
    let mut thread = [obj + 16, base + 4096, base, 7, announce, 8 << 32];
    let mut out = [0u64; 4];
    // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64, u64) -> u64`
    // that reads `thread` and `tab`, writes `thread[0]`, one `bitmap` word
    // and `out`.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(buf.as_ptr())
    };
    let r = f(thread.as_mut_ptr() as u64, out.as_mut_ptr() as u64, obj);
    let ret = if r == 0 { 0 } else { r - base };
    Some((ret, thread[0] - base, bitmap[0], out))
}

#[test]
fn the_announced_arm_sets_the_start_bit_and_keeps_the_commit() {
    let (obj, cursor, bits, regs) = run(Some((0, 7))).expect("emits");
    assert_eq!(obj, 64, "RAX = the object");
    assert_eq!(cursor, 80, "the commit stands");
    assert_eq!(bits, 1 << 8, "exactly the start bit of offset 64");
    assert_eq!(regs, SENTINELS);
}

#[test]
fn every_helper_exit_undoes_the_bump() {
    for table in [None, Some((1, 7)), Some((0, 8))] {
        let (obj, cursor, bits, regs) = run(table).expect("emits");
        assert_eq!(obj, 0, "{table:?}: declines to the call");
        assert_eq!(cursor, 64, "{table:?}: the cursor is back at the object");
        assert_eq!(bits, 0, "{table:?}: no start bit");
        assert_eq!(regs, SENTINELS, "{table:?}");
    }
}
