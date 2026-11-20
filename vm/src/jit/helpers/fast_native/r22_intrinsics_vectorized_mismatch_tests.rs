    use super::*;

    /// The word step finds the same first differing byte as a plain scan, at
    /// every position and every length around the 8-byte boundary.
    #[test]
    fn r22_first_differing_byte_matches_a_byte_scan() {
        for len in 0usize..41 {
            let x: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(first_differing_byte(&x, &x), None, "len {len}");
            for at in 0..len {
                let mut y = x.clone();
                y[at] ^= 0x40;
                if at + 1 < len {
                    y[len - 1] ^= 0x01; // a later difference must not win                
}
                assert_eq!(first_differing_byte(&x, &y), Some(at), "len {len} at {at}");            }        }
        // The shorter length bounds the compare.
        assert_eq!(first_differing_byte(&[1, 2, 3], &[1, 2]), None);    }