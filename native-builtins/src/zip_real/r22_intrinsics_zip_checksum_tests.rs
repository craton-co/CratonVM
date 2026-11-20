use super::*;

/// The two exports the JIT's direct helpers call answer exactly what the
/// natives answer: zlib's Adler-32 equals the scalar RFC 1950 loop, and
/// `zlib_crc32` equals the bitwise reflected CRC (complemented on entry and
/// exit, the JDK's public representation), for every length 0..300 and a
/// few running seeds, plus the two published check values.
#[test]
fn r22_zlib_exports_answer_the_natives() {
    let data: Vec<u8> = (0..300u32).map(|i| (i * 131 + 7) as u8).collect();
    for len in 0..data.len() {
        for seed in [1u32, 0x024D_0127, 0xFFF0_FFF0, 0] {
            let s = if seed == 0 { 1 } else { seed % 65521 | ((seed >> 16) % 65521) << 16 };
            assert_eq!(zlib_adler32(s, &data[..len]), adler32_update(s, &data[..len]), "adler len {len}");
        }
        for crc in [0u32, 0xCBF4_3926, 0xFFFF_FFFF] {
            assert_eq!(
                zlib_crc32(crc, &data[..len]),
                !crc32_step(!crc, &data[..len]),
                "crc len {len}"
            );
        }
    }
    assert_eq!(zlib_adler32(1, b"123456789"), 0x091E_01DE);
    assert_eq!(zlib_crc32(0, b"123456789"), 0xCBF4_3926);
}
