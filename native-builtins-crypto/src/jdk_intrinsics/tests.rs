use super::*;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 29)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn pad_md(msg: &[u8], block: usize, len_bytes: usize, big_endian: bool) -> Vec<u8> {
    let mut m = msg.to_vec();
    m.push(0x80);
    while m.len() % block != block - len_bytes {
        m.push(0);
    }
    let bits = (msg.len() as u128) * 8;
    let lb = if big_endian {
        bits.to_be_bytes()
    } else {
        bits.to_le_bytes()
    };
    if big_endian {
        m.extend_from_slice(&lb[16 - len_bytes..]);
    } else {
        m.extend_from_slice(&lb[..len_bytes]);
    }
    m
}

#[test]
fn sha256_matches_the_crate_digest() {
    use sha2::Digest;
    let mut rng = Lcg(1);
    for len in [0usize, 3, 55, 56, 64, 119, 1000] {
        let msg = rng.bytes(len);
        let mut st = [
            0x6a09e667u32,
            0xbb67ae85,
            0x3c6ef372,
            0xa54ff53a,
            0x510e527f,
            0x9b05688c,
            0x1f83d9ab,
            0x5be0cd19,
        ];
        sha256_blocks(&mut st, &pad_md(&msg, 64, 8, true));
        let got: Vec<u8> = st.iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(got, sha2::Sha256::digest(&msg).to_vec(), "len {len}");
    }
}

#[test]
fn sha512_and_sha1_match_the_crate_digests() {
    use sha2::Digest;
    let mut rng = Lcg(2);
    for len in [0usize, 111, 112, 200, 1000] {
        let msg = rng.bytes(len);
        let mut st = [
            0x6a09e667f3bcc908u64,
            0xbb67ae8584caa73b,
            0x3c6ef372fe94f82b,
            0xa54ff53a5f1d36f1,
            0x510e527fade682d1,
            0x9b05688c2b3e6c1f,
            0x1f83d9abfb41bd6b,
            0x5be0cd19137e2179,
        ];
        sha512_blocks(&mut st, &pad_md(&msg, 128, 16, true));
        let got: Vec<u8> = st.iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(got, sha2::Sha512::digest(&msg).to_vec());
        let mut s1 = [
            0x67452301u32,
            0xefcdab89,
            0x98badcfe,
            0x10325476,
            0xc3d2e1f0,
        ];
        sha1_blocks(&mut s1, &pad_md(&msg, 64, 8, true));
        let got: Vec<u8> = s1.iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(got, sha1::Sha1::digest(&msg).to_vec());
    }
}

#[test]
fn md5_matches_rfc1321() {
    let cases: [(&[u8], &str); 3] = [
        (b"", "d41d8cd98f00b204e9800998ecf8427e"),
        (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
        (
            b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ];
    for (msg, hex) in cases {
        let mut st = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
        md5_blocks(&mut st, &pad_md(msg, 64, 8, false));
        let got: String = st
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, hex);
    }
}

#[test]
fn sha3_256_matches_the_crate_digest() {
    use sha3::Digest;
    let mut rng = Lcg(3);
    for len in [0usize, 135, 136, 500] {
        let msg = rng.bytes(len);
        // SHA3-256: rate 136, suffix 0x06.
        let mut m = msg.clone();
        m.push(0x06);
        while m.len() % 136 != 0 {
            m.push(0);
        }
        let last = m.len() - 1;
        m[last] |= 0x80;
        let mut st = [0u64; 25];
        sha3_blocks(&mut st, &m, 136);
        let got: Vec<u8> = st[..4].iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(got, sha3::Sha3_256::digest(&msg).to_vec());
    }
}


fn aes_crate_encrypt(key: &[u8], b: &[u8; 16]) -> [u8; 16] {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let mut blk = aes::Block::clone_from_slice(b);
    match key.len() {
        16 => aes::Aes128::new_from_slice(key)
            .unwrap()
            .encrypt_block(&mut blk),
        24 => aes::Aes192::new_from_slice(key)
            .unwrap()
            .encrypt_block(&mut blk),
        _ => aes::Aes256::new_from_slice(key)
            .unwrap()
            .encrypt_block(&mut blk),
    }
    let mut o = [0u8; 16];
    o.copy_from_slice(&blk);
    o
}

#[test]
fn aes_java_schedule_matches_fips197_both_paths() {
    let mut rng = Lcg(4);
    for klen in [16usize, 24, 32] {
        for _ in 0..20 {
            let key = rng.bytes(klen);
            let (ke, kd, rounds) = portable::make_session_key(&key);
            let enc = AesRoundKeys::for_encrypt(&ke, (rounds * 4) as i32).unwrap();
            let dec = AesRoundKeys::for_decrypt(&kd, rounds >= 12, rounds == 14).unwrap();
            let mut pt = [0u8; 16];
            pt.copy_from_slice(&rng.bytes(16));
            let want = aes_crate_encrypt(&key, &pt);
            assert_eq!(enc.apply_block_portable(&pt), want);
            let mut b = pt;
            enc.apply_block(&mut b);
            assert_eq!(b, want);
            assert_eq!(dec.apply_block_portable(&want), pt);
            let mut c = want;
            dec.apply_block(&mut c);
            assert_eq!(c, pt);
            // bulk path (8-wide) equals block path
            let mut many = rng.bytes(16 * 19);
            let mut one = many.clone();
            enc.apply_blocks(&mut many);
            for blk in one.chunks_exact_mut(16) {
                let mut x = [0u8; 16];
                x.copy_from_slice(blk);
                blk.copy_from_slice(&enc.apply_block_portable(&x));
            }
            assert_eq!(many, one);
        }
    }
}

#[test]
fn aes_arbitrary_k_arrays_agree_across_paths() {
    let mut rng = Lcg(5);
    for limit in [4i32, 8, 40, 48, 56, 41] {
        let k: Vec<i32> = (0..80).map(|_| rng.next() as i32).collect();
        let enc = AesRoundKeys::for_encrypt(&k, limit).unwrap();
        for flags in [(false, false), (true, false), (true, true), (false, true)] {
            let dec = AesRoundKeys::for_decrypt(&k, flags.0, flags.1).unwrap();
            let mut x = [0u8; 16];
            x.copy_from_slice(&rng.bytes(16));
            let mut a = x;
            dec.apply_block(&mut a);
            assert_eq!(a, dec.apply_block_portable(&x));
        }
        let mut x = [0u8; 16];
        x.copy_from_slice(&rng.bytes(16));
        let mut a = x;
        enc.apply_block(&mut a);
        assert_eq!(a, enc.apply_block_portable(&x));
    }
    assert_eq!(AesRoundKeys::for_encrypt(&[0; 43], 40).err(), Some(43));
    assert_eq!(
        AesRoundKeys::for_decrypt(&[0; 3], false, false).err(),
        Some(4)
    );
}

#[test]
fn ctr_keystream_matches_the_byte_loop() {
    let mut rng = Lcg(6);
    let key = rng.bytes(16);
    let (ke, _, _) = portable::make_session_key(&key);
    let enc = AesRoundKeys::for_encrypt(&ke, 40).unwrap();
    for len in [0usize, 1, 15, 16, 17, 100, 257] {
        for used0 in [0i32, 5, 16] {
            let mut c1 = [0xffu8; 16];
            c1[0] = 7;
            let mut e1 = [0u8; 16];
            e1.copy_from_slice(&rng.bytes(16));
            let (mut c2, mut e2, mut u1, mut u2) = (c1, e1, used0, used0);
            let fast = ctr_keystream(&enc, &mut c1, &mut e1, &mut u1, len);
            let mut slow = Vec::new();
            for _ in 0..len {
                if u2 >= 16 {
                    e2 = enc.apply_block_portable(&c2);
                    ctr_increment(&mut c2);
                    u2 = 0;
                }
                slow.push(e2[u2 as usize]);
                u2 += 1;
            }
            assert_eq!(fast, slow);
            assert_eq!((c1, e1, u1), (c2, e2, u2), "len {len} used {used0}");
        }
    }
}

#[test]
fn ghash_matches_the_java_bit_loop() {
    let mut rng = Lcg(7);
    for n in [0usize, 1, 2, 5, 33] {
        let h = [rng.next(), rng.next()];
        let s = [rng.next(), rng.next()];
        let data = rng.bytes(16 * n);
        let mut a = s;
        let mut b = s;
        ghash_blocks(&mut a, h, &data);
        ghash_blocks_reference(&mut b, h, &data);
        assert_eq!(a, b, "n {n}");
    }
}

#[test]
fn chacha20_block_matches_rfc8439_2_3_2() {
    let key: Vec<u8> = (0u8..32).collect();
    let nonce = [0u8, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut init = [0u32; 16];
    for i in 0..8 {
        init[4 + i] =
            u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    init[12] = 1;
    for i in 0..3 {
        init[13 + i] = u32::from_le_bytes([
            nonce[4 * i],
            nonce[4 * i + 1],
            nonce[4 * i + 2],
            nonce[4 * i + 3],
        ]);
    }
    let mut out = [0u8; 128];
    chacha20_blocks(&init, &mut out);
    let want_head = [
        0x10u8, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f,
    ];
    assert_eq!(&out[..12], &want_head);
    let mut second = [0u8; 64];
    let mut init2 = init;
    init2[12] = 2;
    chacha20_blocks(&init2, &mut second);
    assert_eq!(&out[64..], &second[..]);
}

/// Value of (non-negative, carried) 26-bit limbs mod 2^130-5.
fn limbs_value(l: &[i64; 5]) -> num_bigint::BigUint {
    use num_bigint::BigUint;
    let p = (BigUint::from(1u8) << 130usize) - BigUint::from(5u8);
    let mut v = BigUint::from(0u8);
    for (i, x) in l.iter().enumerate() {
        assert!(*x >= 0, "limb {i} negative: {x}");
        v += BigUint::from(*x as u64) << (26 * i);
    }
    v % p
}

#[test]
fn poly1305_limbs_compute_horner_mod_p() {
    use num_bigint::BigUint;
    let mut rng = Lcg(8);
    let p = (BigUint::from(1u8) << 130usize) - BigUint::from(5u8);
    // r clamped, 26-bit limbs as IntegerPolynomial1305.getElement leaves them.
    let mut rb = rng.bytes(16);
    for i in [3usize, 7, 11, 15] {
        rb[i] &= 15;
    }
    for i in [4usize, 8, 12] {
        rb[i] &= 252;
    }
    let r_int = BigUint::from_bytes_le(&rb);
    let mut r16 = [0u8; 16];
    r16.copy_from_slice(&rb);
    let rv = u128::from_le_bytes(r16);
    let mut r = [0i64; 5];
    for (i, x) in r.iter_mut().enumerate() {
        *x = ((rv >> (26 * i)) & 0x3ff_ffff) as i64;
    }
    let data = rng.bytes(16 * 9);
    let mut st = Poly1305Limbs {
        a: [0; 5],
        a_num_adds: 0,
        r,
        r_num_adds: 0,
        r_changed: false,
    };
    poly1305_blocks(&mut st, &data);
    let mut acc = BigUint::from(0u8);
    for blk in data.chunks_exact(16) {
        let mut b = blk.to_vec();
        b.push(1);
        acc = ((acc + BigUint::from_bytes_le(&b)) * &r_int) % &p;
    }
    assert_eq!(limbs_value(&st.a), acc);
    assert_eq!(st.a_num_adds, 0);
    assert!(!st.r_changed);
}
