use super::*;

#[cfg(windows)]
const A0: u8 = 1; // RCX
#[cfg(not(windows))]
const A0: u8 = 7; // RDI

const OBJECT_CID: u32 = 3;
const STRING_CID: u32 = 77;

/// A fake header: class id `cid` at offset 0, kind tags byte `tags`.
fn header(cid: u32, tags: u8) -> Vec<u64> {
    let mut raw = vec![0u8; 64];
    raw[..4].copy_from_slice(&cid.to_le_bytes());
    raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = tags;
    raw.chunks(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
        .collect()
}

/// 1 when the screen passes RDX = `obj`, -1 when it declines.
fn screen(obj: u64) -> i64 {
    let mut buf = ExecutableBuffer::new(1 << 12).expect("test buffer");
    mov_r64_r64(&mut buf, RDX, A0);
    let mut decline = Vec::new();
    emit_ir_object_array_screen(&mut buf, OBJECT_CID, &mut decline);
    mov_imm32_sx(&mut buf, RAX, 1);
    buf.emit_byte(0xC3); // RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit_byte(0xC3); // RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet reading the caller's buffer (alive across
    // the call) through caller-saved registers only.
    let f: extern "C" fn(u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    f(obj)
}

#[test]
fn rx27_only_an_exact_object_array_passes_the_add_screen() {
    let array = cratonvm_types::ObjectKind::Array as u8;
    let ref_tags = array | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    let byte_tags = cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B");
    let objects = header(OBJECT_CID, ref_tags);
    let strings = header(STRING_CID, ref_tags);
    let bytes = header(0, byte_tags);
    let plain = header(OBJECT_CID, 0);
    // Cast (all): the fake headers' addresses.
    assert_eq!(screen(objects.as_ptr() as u64), 1, "Object[]");
    assert_eq!(
        screen(strings.as_ptr() as u64),
        -1,
        "String[]: the aastore's ASE"
    );
    assert_eq!(screen(bytes.as_ptr() as u64), -1, "byte[]");
    assert_eq!(screen(plain.as_ptr() as u64), -1, "a plain object");
    assert_eq!(screen(0), -1, "null");
}
