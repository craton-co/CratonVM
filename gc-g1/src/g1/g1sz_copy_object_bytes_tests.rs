#[test]
fn g1sz_every_length_copies_exactly_its_bytes() {
    let src: Vec<u8> = (0..96u8).map(|b| b.wrapping_mul(7).wrapping_add(1)).collect();
    for len in 0..=80usize {
        let mut dst = vec![0xEEu8; 96];
        // SAFETY: both buffers hold 96 bytes and are distinct.
        unsafe { super::copy_object_bytes(src.as_ptr(), dst.as_mut_ptr(), len) };
        assert_eq!(&dst[..len], &src[..len], "len {len}");
        assert!(dst[len..].iter().all(|&b| b == 0xEE), "len {len} wrote past its end");
    }
}
