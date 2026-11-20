use super::{
    getclass_prim_array_lookup_bytes, getclass_prim_array_slot, GETCLASS_WORDS_PRIM_ARRAY_BASE,
    GETCLASS_WORDS_PRIM_ARRAY_CAPACITY, GETCLASS_WORDS_REF_ARRAY_BASE, GETCLASS_WORDS_TOTAL,
};
use cratonvm_types::{primitive_array_kind_tags_byte, ArrayElementType as E};

const PRIMS: [(&str, E); 8] = [
    ("[Z", E::Boolean),
    ("[C", E::Char),
    ("[F", E::Float),
    ("[D", E::Double),
    ("[B", E::Byte),
    ("[S", E::Short),
    ("[I", E::Int),
    ("[J", E::Long),
];

/// The band is the table's tail and the slot mapping is a bijection onto
/// it; a reference array has no slot.
#[test]
fn the_prim_array_band_is_the_tables_tail() {
    assert_eq!(
        GETCLASS_WORDS_PRIM_ARRAY_BASE + GETCLASS_WORDS_PRIM_ARRAY_CAPACITY,
        // nio21 wave 2: the reference-array band now follows the primitive one.
        GETCLASS_WORDS_REF_ARRAY_BASE
    );
    let mut seen = [false; GETCLASS_WORDS_PRIM_ARRAY_CAPACITY];
    for (_, e) in PRIMS {
        let s = getclass_prim_array_slot(e).expect("primitive slot");
        assert!(!seen[s]);
        seen[s] = true;
    }
    assert_eq!(getclass_prim_array_slot(E::Reference), None);
}

/// The array arm, EXECUTED over fake headers: every one-dimensional
/// primitive array finds its own element type's word; a reference array
/// (`Object[]`, `int[][]`), a humongous filler, a plain object's byte,
/// junk element tags and an unpublished word decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_array_arm_answers_each_primitive_array_and_declines_the_rest() {
    use crate::ExecutableBuffer;
    let mut table = vec![0usize; GETCLASS_WORDS_TOTAL];
    for (_, e) in PRIMS {
        if e == E::Short {
            continue; // left unpublished
        }
        let s = getclass_prim_array_slot(e).unwrap();
        table[GETCLASS_WORDS_PRIM_ARRAY_BASE + s] = 0x3000_0000 + 8 * s;
    }
    let (bytes, decline) = getclass_prim_array_lookup_bytes();
    // MOV RAX, arg0 ; MOV R10, arg1 ; <lookup> ; RET ; decline: MOV RAX,-1 ; RET
    let (a0, a1) = (super::CALL_ARG_REGS[0], super::CALL_ARG_REGS[1]);
    // MOV RAX, a0: REX.W [+R] 89 /r (a0 in the reg field).
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
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: a complete `extern "C" fn(u64, u64) -> u64` that reads one
    // header byte and the table and writes only RAX and R10.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    let t = table.as_ptr() as u64;
    let call = |tag: u8| {
        // A fake 16-byte header (x86-64 is little-endian): class id 0,
        // the tag at KIND_TAGS.
        let header = [
            u64::from(tag) << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET),
            0u64,
        ];
        f(header.as_ptr() as u64, t) as i64
    };
    for (desc, e) in PRIMS {
        let tag = primitive_array_kind_tags_byte(desc).unwrap();
        let s = getclass_prim_array_slot(e).unwrap();
        let want = if e == E::Short {
            -1
        } else {
            table[GETCLASS_WORDS_PRIM_ARRAY_BASE + s] as i64
        };
        assert_eq!(call(tag), want, "{desc}");
    }
    let reference_array = cratonvm_types::ObjectKind::Array as u8;
    let filler = cratonvm_types::ObjectKind::HumongousFiller as u8;
    for tag in [
        reference_array,
        filler,
        0u8,
        0x31,
        0x35,
        0x3D,
        0x41,
        0xC0 | 0x29,
        0xFF,
    ] {
        assert_eq!(call(tag), -1, "tag {tag:#x} must decline");
    }
}
