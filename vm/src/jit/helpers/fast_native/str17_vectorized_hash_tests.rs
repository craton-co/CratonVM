use super::*;

/// `String.hashCode()` of `s` as the JLS defines it.
fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)))
}

/// The fold answers `String.hashCode()` for both string codings
/// (`StringLatin1` passes `T_BOOLEAN`, `StringUTF16` a `byte[]` of LE
/// pairs with `T_CHAR`), `Arrays.hashCode` for the signed element types
/// (seeded with 1), and refuses a pair the JDK never passes.
#[test]
fn str17_the_fold_answers_the_jdk_bodies() {
    let latin1 = "k1999999-13999993\u{e9}";
    let bytes: Vec<u8> = latin1.chars().map(|c| c as u32 as u8).collect();
    assert_eq!(
        vectorized_hash_fold(&bytes, 1, VH_T_BOOLEAN, 0),
        Some(java_string_hash(latin1))
    );
    let utf16 = "\u{3a3}\u{39f}\u{3a3}k";
    let pairs: Vec<u8> = utf16.encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert_eq!(
        vectorized_hash_fold(&pairs, 2, VH_T_CHAR, 0),
        Some(924_359 * 31 + 107)
    );
    assert_eq!(
        vectorized_hash_fold(&pairs, 2, VH_T_CHAR, 0),
        Some(java_string_hash(utf16))
    );
    // `Arrays.hashCode(byte[]{-1, 2})`: signed, seeded with 1.
    assert_eq!(
        vectorized_hash_fold(&[0xff, 2], 1, VH_T_BYTE, 1),
        Some((31 + -1) * 31 + 2)
    );
    // `Arrays.hashCode(short[]{-2})` and `int[]{i32::MIN}`.
    assert_eq!(
        vectorized_hash_fold(&(-2i16).to_le_bytes(), 2, VH_T_SHORT, 1),
        Some(31 - 2)
    );
    assert_eq!(
        vectorized_hash_fold(&i32::MIN.to_le_bytes(), 4, VH_T_INT, 1),
        Some(31i32.wrapping_add(i32::MIN))
    );
    assert_eq!(vectorized_hash_fold(&[], 1, VH_T_BOOLEAN, 7), Some(7));
    assert_eq!(vectorized_hash_fold(&[1, 2], 2, VH_T_INT, 1), None);
}

#[test]
fn str17_unrolled8_matches_one_at_a_time_across_lengths_0_to_70() {
    for len in 0..=70usize {
        for seed in [0i32, 1, -37] {
            let vals: Vec<i32> = (0..len)
                .map(|i| (i as i32).wrapping_mul(257).wrapping_sub(129))
                .collect();
            let want = vals
                .iter()
                .fold(seed, |h, &v| h.wrapping_mul(31).wrapping_add(v));
            let got = hash_fold_unrolled8(len, seed, |i| vals[i]);
            assert_eq!(got, want, "len={len} seed={seed}");
        }
    }
}
