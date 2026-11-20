use super::*;

const DECLINED: i64 = -1;

fn mirror(cid: u32) -> u64 {
    0x7300_0000 + u64::from(cid) * 0x158
}

/// `extern "C" fn(mirror) -> class id | -1`, with no JVMTI event word.
fn build(table: usize) -> crate::ExecutableBuffer {
    build_with(table, 0)
}

/// [`build`] testing the JVMTI event word at `jvmti_word` (`0`: none).
fn build_with(table: usize, jvmti_word: usize) -> crate::ExecutableBuffer {
    let (bytes, decline) = reflect_new_array_resolve_bytes(table, jvmti_word);
    // MOV RDX, arg0 (REX.W 89 /r; both ABIs' arg0 is a low register).
    let arg0 = ENTRY_ABI_REGS[0];
    assert!(arg0 < 8);
    let mut body: Vec<u8> = vec![0x48, 0x89, 0xC0 | (arg0 << 3) | 2];
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0x48, 0x89, 0xC8, 0xC3]); // MOV RAX, RCX ; RET
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
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

fn call(buf: &crate::ExecutableBuffer, m: u64) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64) -> i64` that reads only the
    // fake table the test keeps alive and writes RAX, RCX, RDX, R10, R11.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr()) };
    f(m)
}

#[test]
fn a_proven_hint_answers_and_the_rest_declines() {
    const A: u32 = 0x120;
    const B: u32 = 0x3456;
    const REFUSED: u32 = 0x77;
    const STALE: u32 = 0x99;
    let mut table = vec![0usize; GETCLASS_WORDS_TOTAL].into_boxed_slice();
    table[0] = GETCLASS_WORDS_CLASS_CAPACITY;
    let word = |w: u64| usize::try_from(w).expect("64-bit");
    let at = |m: u64| GETCLASS_WORDS_MIRROR_HINT_BASE + mirror_hint_slot(word(m));
    let slots: std::collections::HashSet<usize> =
        [A, B, REFUSED, STALE].iter().map(|&c| at(mirror(c))).collect();
    assert_eq!(slots.len(), 4, "the test's mirrors must not collide");
    for (cid, miss_ok) in [(A, true), (B, false)] {
        table[1 + usize::try_from(cid).expect("small")] = word(mirror(cid));
        table[at(mirror(cid))] = word(mirror_hint_word(cid, miss_ok));
    }
    // An array / primitive class: a remembered refusal, no present bit.
    table[1 + usize::try_from(REFUSED).expect("small")] = word(mirror(REFUSED));
    table[at(mirror(REFUSED))] = word(MIRROR_HINT_DECLINED | u64::from(REFUSED));
    // A hint whose class word is another mirror (moved / re-used).
    table[1 + usize::try_from(STALE).expect("small")] = word(mirror(A));
    table[at(mirror(STALE))] = word(mirror_hint_word(STALE, true));
    let buf = build(table.as_ptr() as usize); // Cast: the table's address
    assert_eq!(call(&buf, mirror(A)), i64::from(A));
    assert_eq!(call(&buf, mirror(B)), i64::from(B), "a synthetic target too");
    assert_eq!(call(&buf, 0), DECLINED, "null: the NPE");
    assert_eq!(call(&buf, mirror(REFUSED)), DECLINED, "no present bit");
    assert_eq!(call(&buf, mirror(STALE)), DECLINED, "the class word is not the mirror");
    assert_eq!(call(&buf, 0x7F00_0008), DECLINED, "an empty slot");
    // A hint past the class band (a lambda-proxy id) declines.
    let lambda = GETCLASS_WORDS_LAMBDA_BASE_ID + 3;
    let m = mirror(0x500);
    table[at(m)] = word(mirror_hint_word(lambda, true));
    assert_eq!(call(&buf, m), DECLINED, "past the class band");
    drop(table);
}

/// j27 wave 4 (lane CLONEIR; page
/// `j27-reflect-new-array-prefix-posts-no-vmobjectalloc-FIXED-20261029.md`):
/// while a C JVMTI env listens for `VMObjectAlloc` or `SampledObjectAlloc`
/// a proven hint DECLINES (the helper declines to the native, which posts
/// the event); any other armed event, and no word at all, still answers.
#[test]
fn j27_an_armed_allocation_event_declines_a_proven_hint() {
    use std::sync::atomic::{AtomicU32, Ordering};
    const A: u32 = 0x120;
    let mut table = vec![0usize; GETCLASS_WORDS_TOTAL].into_boxed_slice();
    table[0] = GETCLASS_WORDS_CLASS_CAPACITY;
    let word = |w: u64| usize::try_from(w).expect("64-bit");
    let hint = GETCLASS_WORDS_MIRROR_HINT_BASE + mirror_hint_slot(word(mirror(A)));
    table[1 + usize::try_from(A).expect("small")] = word(mirror(A));
    table[hint] = word(mirror_hint_word(A, true));
    let events = Box::new(AtomicU32::new(0));
    let addr = &*events as *const AtomicU32 as usize; // Cast: the event word's address
    let buf = build_with(table.as_ptr() as usize, addr); // Cast: the table's address
    for (armed, expect, why) in [
        (0u32, i64::from(A), "nothing armed"),
        (1, i64::from(A), "FieldWatch only"),
        (4, DECLINED, "VMObjectAlloc"),
        (8, DECLINED, "SampledObjectAlloc"),
        (4 | 8 | 2, DECLINED, "both, and Monitor"),
    ] {
        events.store(armed, Ordering::Relaxed);
        assert_eq!(call(&buf, mirror(A)), expect, "{why}");
    }
    // No word published (a build without the JVMTI surface): no test.
    let unwatched = build(table.as_ptr() as usize); // Cast: the table's address
    events.store(4, Ordering::Relaxed);
    assert_eq!(call(&unwatched, mirror(A)), i64::from(A));
    drop((table, events));
}
