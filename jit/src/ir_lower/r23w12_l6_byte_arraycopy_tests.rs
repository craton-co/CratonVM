use super::*;


/// The loops RUN: every length 0..=40 copies exactly `len` bytes and
/// touches nothing past them.
#[test]
fn the_byte_copy_loops_copy_exactly_len_bytes() {
    let mut buf = ExecutableBuffer::new(128).expect("buffer");
    // Move the three C arguments to R10 (src), RCX (dst), R11 (len).
    #[cfg(target_os = "windows")]
    buf.emit(&[0x49, 0x89, 0xCA, 0x48, 0x89, 0xD1, 0x4D, 0x89, 0xC3]); // RCX, RDX, R8
    #[cfg(not(target_os = "windows"))]
    buf.emit(&[0x49, 0x89, 0xFA, 0x48, 0x89, 0xF1, 0x49, 0x89, 0xD3]); // RDI, RSI, RDX
    buf.emit(&BYTE_ARRAYCOPY_LOOPS);
    buf.emit(&[0xC3]); // RET
    buf.finalize();
    // SAFETY: the buffer holds the function just emitted, now executable;
    // it reads `len` bytes at `src`, writes `len` at `dst`, touches only
    // volatile registers and returns.
    let f: extern "C" fn(*const u8, *mut u8, i64) =
        unsafe { std::mem::transmute(buf.as_ptr()) };
    for len in 0..=40usize {
        let src: Vec<u8> = (0..64u8).map(|b| b.wrapping_mul(7).wrapping_add(1)).collect();
        let mut dst = vec![0xAAu8; 64];
        // Cast: a small test length.
        f(src[3..].as_ptr(), dst[5..].as_mut_ptr(), len as i64);
        assert_eq!(&dst[5..5 + len], &src[3..3 + len], "len {len}");
        assert!(dst[..5].iter().all(|&b| b == 0xAA), "len {len}: before");
        assert!(dst[5 + len..].iter().all(|&b| b == 0xAA), "len {len}: after");
    }
}

#[test]
fn the_arm_admits_exactly_byte_and_boolean_arrays() {
    let byte = cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B");
    let boolean = cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z");
    assert_ne!(byte, 0);
    assert_ne!(boolean, 0);
    assert_ne!(byte, boolean);
    for other in ["[C", "[S", "[I", "[J", "[F", "[D"] {
        let tag = cratonvm_types::primitive_array_kind_tags_byte(other).expect("tag");
        assert!(tag != byte && tag != boolean, "{other}");
    }
}
