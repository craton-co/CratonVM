use super::{
    call_entry_null_guard_answer_word, call_entry_null_guard_bytes,
    call_entry_null_guard_decode, ir_call_entry_null_guard_enabled, CALL_ARG_REGS,
};

const DISP: i32 = 24;
const SKIPPED: i64 = 1;
const CALLED: i64 = -1;

#[test]
fn r21w16_cf_the_answer_word_round_trips_and_rejects_foreign_words() {
    for (disp, volatile) in [(8u32, false), (24, true), (0x7FFF_FFF8, true)] {
        let w = call_entry_null_guard_answer_word(disp, volatile);
        assert_ne!(w, 0);
        assert_eq!(
            call_entry_null_guard_decode(w),
            Some((i32::try_from(disp).expect("small"), volatile))
        );
    }
    assert_eq!(call_entry_null_guard_decode(0), None, "no answer");
    assert_eq!(call_entry_null_guard_decode(1 << 40), None, "a zero displacement");
    assert_eq!(call_entry_null_guard_decode((1 << 40) | (2 << 32) | 8), None, "unknown flag");
    assert_eq!(call_entry_null_guard_decode(42), None, "a class id is not an answer");
}


/// `extern "C" fn(_, receiver) -> SKIPPED | CALLED` around the bytes.
fn build() -> crate::ExecutableBuffer {
    let (bytes, decline) = call_entry_null_guard_bytes(DISP);
    let mut body: Vec<u8> = Vec::new();
    // MOV RAX, arg1.
    let src = CALL_ARG_REGS[1];
    body.extend_from_slice(&[0x48 | (u8::from(src >= 8) << 2), 0x89, 0xC0 | ((src & 7) << 3)]);
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV RAX, 1; RET
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1; RET
    for d in decline {
        let operand = at + d;
        let rel = i32::try_from(decline_at).expect("small")
            - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(512).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run(buf: &crate::ExecutableBuffer, recv: u64) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` that reads only
    // the fake object the caller keeps alive and writes RAX and the flags.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
    };
    f(0, recv)
}

#[repr(C, align(16))]
struct Obj([u8; 64]);

fn obj(compact: bool, field: u64) -> Box<Obj> {
    let mut o = Box::new(Obj([0; 64]));
    if compact {
        o.0[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_COMPACT;
    }
    let d = usize::try_from(DISP).expect("small");
    o.0[d..d + 8].copy_from_slice(&field.to_le_bytes());
    o
}

fn addr(o: &Obj) -> u64 {
    o as *const Obj as u64 // Cast: the fake object's address
}

#[test]
fn r21w16_cf_the_guard_skips_only_a_null_field_of_a_compact_receiver() {
    let buf = build();
    let empty = obj(true, 0);
    assert_eq!(run(&buf, addr(&empty)), SKIPPED, "stack == null: no call");
    let pending = obj(true, 0x1000);
    assert_eq!(run(&buf, addr(&pending)), CALLED, "stack != null: the callee has work");
    assert_eq!(run(&buf, 0), CALLED, "a null receiver: the call's NPE");
    let legacy = obj(false, 0);
    assert_eq!(run(&buf, addr(&legacy)), CALLED, "a legacy instance");
    let mut array = obj(true, 0);
    array.0[cratonvm_types::KIND_TAGS_BYTE_OFFSET] |= cratonvm_types::KIND_TAG_BYTE_MASK;
    assert_eq!(run(&buf, addr(&array)), CALLED, "not a plain object");
}
