use super::*;
use crate::x64::ZgcAnnounceCommit;

const R8: u8 = 8;
const R9: u8 = 9;
const SENTINELS: [u64; 4] = [
    0x1111_2222_3333_4444,
    0x5555_6666_7777_8888,
    0x9999_AAAA_BBBB_CCCC,
    0xDDDD_EEEE_FFFF_0123,
];

#[repr(C, align(512))]
struct Chunk([u64; 512]);

/// The announce table (`words, base, span, blocked, epoch`) over `chunk`
/// and `bitmap`, with `refuse` applied: `None` keeps it valid.
fn table(chunk: &Chunk, bitmap: &mut [u64; 8], refuse: Option<&str>) -> [u64; 8] {
    let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
    let mut tab = [bitmap.as_mut_ptr() as u64, base, 4096, 0, 7, 0, 0, 0]; // Cast: the bitmap's address
    match refuse {
        Some("blocked") => {
            tab[3] = 1;
            tab[6] = 1; // short_blocked is raised with blocked
        }
        Some("epoch") => tab[4] = 8,
        Some("unpublished") => tab[0] = 0,
        _ => {}
    }
    tab
}

/// `extern "C" fn(thread, obj) -> obj | 0` around the committing store
/// alone: R10 = thread, R11 = obj; `0` on every refusal.
fn build_store(commit: ZgcAnnounceCommit) -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(1024)?;
    buf.emit(&[0x55, 0x48, 0x89, 0xE5]); // PUSH RBP; MOV RBP, RSP
    emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
    emit_mov_reg(&mut buf, R11, ENTRY_ABI_REGS[1]);
    let (done, to_helper) =
        crate::x64::emit_zgc_inline_announce_raw_committing(&mut buf, 0, 8, commit);
    for p in to_helper {
        patch_rel32_to_here(&mut buf, p);
    }
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    patch_rel32_to_here(&mut buf, done);
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    if buf.overflowed() {
        return None;
    }
    buf.finalize();
    Some(buf)
}

/// `(returned offset or 0, cursor offset after, bitmap word 0)` for an
/// object 64 bytes into a 4 KiB, 512-aligned TLAB whose cursor is STILL at
/// the object (header written, nothing committed). `array_len` is written
/// as the header's length when `Some`.
fn run_store(
    commit: ZgcAnnounceCommit,
    refuse: Option<&str>,
    array_len: Option<u32>,
) -> Option<(u64, u64, u64)> {
    let buf = build_store(commit)?;
    let mut chunk = Box::new(Chunk([0; 512]));
    if let Some(len) = array_len {
        let word = 8 + cratonvm_types::ARRAY_LENGTH_OFFSET / 8;
        let shift = 8 * (cratonvm_types::ARRAY_LENGTH_OFFSET % 8);
        chunk.0[word] |= u64::from(len) << shift;
    }
    let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
    let obj = base + 64;
    let mut bitmap = [0u64; 8];
    let tab = table(&chunk, &mut bitmap, refuse);
    // cursor (= the object: uncommitted), end, start, owned epoch, table.
    // Cast: the table's address.
    // zr z11/ia: plus the owned-word window, words [0, 8).
    let mut thread = [obj, base + 4096, base, 7, tab.as_ptr() as u64, 8 << 32];
    // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64) -> u64` that
    // reads `thread`, `tab` and the chunk's header, and writes `thread[0]`
    // and one `bitmap` word.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    let r = f(thread.as_mut_ptr() as u64, obj); // Cast: the thread block's address
    let ret = if r == 0 { 0 } else { r - base };
    Some((ret, thread[0] - base, bitmap[0]))
}

#[test]
fn the_store_sets_the_bit_then_commits_the_size() {
    let (obj, cursor, bits) =
        run_store(ZgcAnnounceCommit::Size(24), None, None).expect("emits");
    assert_eq!(obj, 64, "RAX = the object");
    assert_eq!(cursor, 64 + 24, "committed after the bit");
    assert_eq!(bits, 1 << 8, "exactly the start bit of offset 64");
}

#[test]
fn the_array_commit_is_re_derived_from_the_header_length() {
    // int[5]: 20 data bytes, rounded to 24.
    let (obj, cursor, bits) = run_store(
        ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 2 },
        None,
        Some(5),
    )
    .expect("emits");
    assert_eq!(obj, 64);
    assert_eq!(cursor as usize, 64 + cratonvm_types::ARRAY_DATA_OFFSET + 24); // Cast: small
    assert_eq!(bits, 1 << 8);
    // byte[9]: 9 rounded to 16, no shift.
    let (_, cursor, _) = run_store(
        ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 0 },
        None,
        Some(9),
    )
    .expect("emits");
    assert_eq!(cursor as usize, 64 + cratonvm_types::ARRAY_DATA_OFFSET + 16);
    // Cast: small
}

#[test]
fn a_refusal_leaves_the_bitmap_and_the_cursor_untouched() {
    for refuse in ["blocked", "epoch", "unpublished"] {
        let (obj, cursor, bits) =
            run_store(ZgcAnnounceCommit::Size(24), Some(refuse), None).expect("emits");
        assert_eq!(obj, 0, "{refuse}: to the helper arm");
        assert_eq!(
            cursor, 64,
            "{refuse}: nothing committed, nothing to roll back"
        );
        assert_eq!(bits, 0, "{refuse}: no start bit");
    }
}

#[test]
fn already_committed_emits_the_round_16_sequence_byte_for_byte() {
    let mut a = ExecutableBuffer::new(1024).expect("buffer");
    let (done_a, help_a) = crate::x64::emit_zgc_inline_announce_raw(&mut a, 0x40, 0x48);
    let mut b = ExecutableBuffer::new(1024).expect("buffer");
    let (done_b, help_b) = crate::x64::emit_zgc_inline_announce_raw_committing(
        &mut b,
        0x40,
        0x48,
        ZgcAnnounceCommit::AlreadyCommitted,
    );
    assert_eq!(a.as_slice(), b.as_slice());
    assert_eq!((done_a, help_a), (done_b, help_b));
}

/// The legacy plan the box prefixes build, for a class with no layout.
fn box_plan() -> InlineTlabPlan {
    InlineTlabPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        post_init: 0x1000,
        context_off: 16,
        class_id: 915_160,
        num_fields: 1,
        announce: 0,
        skip_post_init: true,
        register_first: 0,
        post_init_helper_first: false,
    }
}

/// `extern "C" fn(thread, out) -> obj | 0` around the box prefix's
/// announced fast path; `out` receives RCX, RDX, R8, R9 on both arms.
fn build_box() -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(2048)?;
    // PUSH RBP; MOV RBP, RSP; SUB RSP, 32. Thread at [RBP-8], out at [RBP-16].
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x20]);
    emit_store_frame(&mut buf, 8, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, 16, ENTRY_ABI_REGS[1]);
    for (reg, v) in [RCX, RDX, R8, R9].into_iter().zip(SENTINELS) {
        emit_mov_imm64(&mut buf, reg, v);
    }
    let mut decline = Vec::new();
    if !emit_inline_tlab_fast_path_announced_ir(&mut buf, &box_plan(), 0x1000, &mut decline) {
        return None;
    }
    let announced = emit_jmp(&mut buf);
    for p in decline {
        patch_rel32_to_here(&mut buf, p);
    }
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    patch_rel32_to_here(&mut buf, announced);
    emit_load_frame(&mut buf, R10, 16);
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

/// `(returned offset or 0, cursor offset after, bitmap word 0, registers)`
/// for a box bumped at offset 64 of a fresh 4 KiB TLAB.
fn run_box(refuse: Option<&str>) -> Option<(u64, u64, u64, [u64; 4])> {
    let buf = build_box()?;
    let chunk = Box::new(Chunk([0; 512]));
    let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
    let mut bitmap = [0u64; 8];
    let tab = table(&chunk, &mut bitmap, refuse);
    // Cast: the table's address.
    // zr z11/ia: plus the owned-word window, words [0, 8).
    let mut thread = [
        base + 64,
        base + 4096,
        base,
        7,
        tab.as_ptr() as u64,
        8 << 32,
    ];
    let mut out = [0u64; 4];
    // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64) -> u64` that
    // reads `thread` and `tab`, writes `thread[0]`, the box's header in
    // `chunk`, one `bitmap` word and `out`.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    let r = f(thread.as_mut_ptr() as u64, out.as_mut_ptr() as u64); // Cast: addresses
    let ret = if r == 0 { 0 } else { r - base };
    drop(chunk);
    Some((ret, thread[0] - base, bitmap[0], out))
}


/// nio21 wave 12 L5: what the stand-in announce helper saw, through the
/// context argument (no process global).
#[repr(C)]
struct HelperProbe {
    thread: *const u64,
    cursor_at_call: u64,
    obj: u64,
    calls: u64,
}

extern "C" fn probe_note(ctx: u64, obj: u64, _size: u64) -> u64 {
    // SAFETY: the test passes a live `&mut HelperProbe` as the context
    // word, whose `thread` points at the live thread block.
    let p = unsafe { &mut *(ctx as *mut HelperProbe) };
    // SAFETY: as above.
    p.cursor_at_call = unsafe { *p.thread };
    p.obj = obj;
    p.calls += 1;
    obj
}





/// zt t10/ca2: what the stand-in announce and post-init helpers saw,
/// through the context argument (no process global).
#[repr(C)]
struct RegisterFirstProbe {
    thread: *const u64,
    answer_zero: bool,
    note_cursor: u64,
    note_calls: u64,
    post_cursor: u64,
    post_obj: u64,
    post_fields: u64,
    post_calls: u64,
    /// 1 = the announce helper ran first, 2 = the post-init helper did.
    first: u64,
}

extern "C" fn rf_note(ctx: u64, obj: u64, _size: u64) -> u64 {
    // SAFETY: the test passes a live `&mut RegisterFirstProbe` as the
    // context word, whose `thread` points at the live thread block.
    let p = unsafe { &mut *(ctx as *mut RegisterFirstProbe) };
    // SAFETY: as above.
    p.note_cursor = unsafe { *p.thread };
    p.note_calls += 1;
    if p.first == 0 {
        p.first = 1;
    }
    if p.answer_zero {
        0
    } else {
        obj
    }
}

extern "C" fn rf_post(ctx: u64, obj: u64, _class_id: u64, fields: u64) -> u64 {
    // SAFETY: as `rf_note`.
    let p = unsafe { &mut *(ctx as *mut RegisterFirstProbe) };
    // SAFETY: as above.
    p.post_cursor = unsafe { *p.thread };
    p.post_obj = obj;
    p.post_fields = fields;
    p.post_calls += 1;
    if p.first == 0 {
        p.first = 2;
    }
    obj
}







