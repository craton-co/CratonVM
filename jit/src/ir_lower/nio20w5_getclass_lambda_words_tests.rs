use super::{
    getclass_table_lookup_bytes, GETCLASS_WORDS_CLASS_CAPACITY, GETCLASS_WORDS_LAMBDA_BASE_ID,
    GETCLASS_WORDS_LAMBDA_CAPACITY, GETCLASS_WORDS_TOTAL,
};

/// The lookup, EXECUTED over a fake table of the real shape: class ids and
/// lambda ids find their own word; ids between the bands, past either
/// band, and unpublished words decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_lookup_answers_both_bands_and_declines_everything_else() {
    use crate::ExecutableBuffer;
    let mut table = vec![0usize; GETCLASS_WORDS_TOTAL];
    table[0] = GETCLASS_WORDS_CLASS_CAPACITY;
    let class_word = |id: usize| 1 + id;
    let lambda_word = |id: u32| {
        1 + GETCLASS_WORDS_CLASS_CAPACITY + (id - GETCLASS_WORDS_LAMBDA_BASE_ID) as usize
    };
    for id in [0usize, 1, 7, GETCLASS_WORDS_CLASS_CAPACITY - 1] {
        table[class_word(id)] = 0x1000_0000 + 8 * id;
    }
    let base = GETCLASS_WORDS_LAMBDA_BASE_ID;
    let lcap = GETCLASS_WORDS_LAMBDA_CAPACITY as u32; // Cast: 1 << 16
    for id in [base, base + 1, base + lcap - 1] {
        table[lambda_word(id)] = 0x2000_0000 + 8 * (id - base) as usize;
    }
    let (bytes, decline) = getclass_table_lookup_bytes();
    // MOV EAX, arg0 ; MOV R10, arg1 ; <lookup> ; RET ; decline: MOV RAX,-1 ; RET
    let (a0, a1) = (super::CALL_ARG_REGS[0], super::CALL_ARG_REGS[1]);
    let mut body: Vec<u8> = Vec::new();
    // MOV r/m32 (EAX), r32 (a0): [REX.R] 89 /r -- zero-extends.
    if a0 >= 8 {
        body.push(0x44);
    }
    body.extend_from_slice(&[0x89, 0xC0 | ((a0 & 7) << 3)]);
    // MOV R10, a1: REX.W + B(R10) [+R] 89 /r.
    body.extend_from_slice(&[
        0x49 | (u8::from(a1 >= 8) << 2),
        0x89,
        0xC2 | ((a1 & 7) << 3),
    ]);
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3);
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    for d in decline {
        let operand = at + d;
        let rel = i32::try_from(decline_at).unwrap() - i32::try_from(operand + 4).unwrap();
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: a complete `extern "C" fn(u64, u64) -> u64` that reads the
    // table and writes only RAX and R10, both caller-saved on both ABIs.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    let t = table.as_ptr() as u64;
    let call = |id: u32| f(0xDEAD_0000_0000_0000 | u64::from(id), t) as i64;
    for id in [0u32, 1, 7, GETCLASS_WORDS_CLASS_CAPACITY as u32 - 1] {
        assert_eq!(
            call(id),
            table[class_word(id as usize)] as i64,
            "class id {id}"
        );
    }
    for id in [base, base + 1, base + lcap - 1] {
        assert_eq!(call(id), table[lambda_word(id)] as i64, "lambda id {id:#x}");
    }
    for id in [
        2u32,     // unpublished class word
        base + 2, // unpublished lambda word
        GETCLASS_WORDS_CLASS_CAPACITY as u32,
        0x7FFF_FFFF,
        base + lcap,
        u32::MAX,
    ] {
        assert_eq!(call(id), -1, "id {id:#x} must decline");
    }
}
