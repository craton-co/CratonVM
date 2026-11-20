use super::{
    getclass_array_arm_bytes, getclass_prim_array_slot, GETCLASS_WORDS_LAMBDA_BASE_ID,
    GETCLASS_WORDS_PRIM_ARRAY_BASE, GETCLASS_WORDS_REF_ARRAY_BASE,
    GETCLASS_WORDS_REF_ARRAY_CAPACITY, GETCLASS_WORDS_TOTAL,
};
use cratonvm_types::{primitive_array_kind_tags_byte, ArrayElementType as E};

/// Run the array arm over a fake 16-byte header (`class_id` at offset 0,
/// `tag` at `KIND_TAGS`): the word it answers, or `-1` for a decline.
fn run(refs: bool, prims: bool, table: &[usize], class_id: u32, tag: u8) -> i64 {
    use crate::ExecutableBuffer;
    let (bytes, decline) = getclass_array_arm_bytes(refs, prims);
    let (a0, a1) = (super::CALL_ARG_REGS[0], super::CALL_ARG_REGS[1]);
    // MOV RAX, a0 ; MOV R10, a1 ; <arm> ; RET ; decline: MOV RAX,-1 ; RET
    let mut body: Vec<u8> = vec![
        0x48 | (u8::from(a0 >= 8) << 2),
        0x89,
        0xC0 | ((a0 & 7) << 3),
    ];
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
    let mut buf = ExecutableBuffer::new(512).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: a complete `extern "C" fn(u64, u64) -> u64` that reads one
    // fake header and the table and writes only RAX and R10.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    let header = [
        u64::from(class_id) | (u64::from(tag) << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET)),
        0u64,
    ];
    f(header.as_ptr() as u64, table.as_ptr() as u64) as i64
}

/// The reference arm, EXECUTED: a reference array finds its component's
/// word; an unpublished component, a component past the band and a lambda
/// component decline; primitive arrays still take the primitive arm (or
/// decline with it off); fillers and junk tags decline in every shape.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_ref_arm_answers_by_component_and_leaves_the_prim_arm_intact() {
    // JIT round 20 wave 19, lane zAssign: the mirror-hint band follows.
    assert_eq!(
        GETCLASS_WORDS_REF_ARRAY_BASE + GETCLASS_WORDS_REF_ARRAY_CAPACITY,
        super::GETCLASS_WORDS_MIRROR_HINT_BASE
    );
    let mut table = vec![0usize; GETCLASS_WORDS_TOTAL];
    let ref_word = |cid: usize| GETCLASS_WORDS_REF_ARRAY_BASE + cid;
    for cid in [0usize, 5, GETCLASS_WORDS_REF_ARRAY_CAPACITY - 1] {
        table[ref_word(cid)] = 0x4000_0000 + 8 * cid;
    }
    let int_slot = getclass_prim_array_slot(E::Int).unwrap();
    table[GETCLASS_WORDS_PRIM_ARRAY_BASE + int_slot] = 0x3000_0000;
    let ref_tag = cratonvm_types::ObjectKind::Array as u8;
    let int_tag = primitive_array_kind_tags_byte("[I").unwrap();
    let filler = cratonvm_types::ObjectKind::HumongousFiller as u8;
    for (refs, prims) in [(true, true), (true, false), (false, true), (false, false)] {
        for cid in [0u32, 5, GETCLASS_WORDS_REF_ARRAY_CAPACITY as u32 - 1] {
            let want = if refs {
                table[ref_word(cid as usize)] as i64
            } else {
                -1
            };
            let got = run(refs, prims, &table, cid, ref_tag);
            assert_eq!(got, want, "ref cid {cid} {refs}/{prims}");
        }
        let cap = GETCLASS_WORDS_REF_ARRAY_CAPACITY as u32; // Cast: 1 << 16
        for cid in [6u32, cap, GETCLASS_WORDS_LAMBDA_BASE_ID, u32::MAX] {
            let got = run(refs, prims, &table, cid, ref_tag);
            assert_eq!(got, -1, "cid {cid:#x} must decline");
        }
        let want_int = if prims { 0x3000_0000 } else { -1 };
        assert_eq!(
            run(refs, prims, &table, 0, int_tag),
            want_int,
            "int[] {refs}/{prims}"
        );
        for tag in [filler, 0u8, 0x41, 0xC1, 0xFF] {
            assert_eq!(
                run(refs, prims, &table, 5, tag),
                -1,
                "tag {tag:#x} must decline"
            );
        }
    }
}
