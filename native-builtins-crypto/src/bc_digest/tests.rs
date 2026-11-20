use super::*;

/// SHA-256 initial chaining value (FIPS 180-4 §5.3.3) — BouncyCastle's
/// `SHA256Digest.reset()`.
const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
    0x5be0cd19,
];

/// Pad `msg` per FIPS 180-4 and compress every block — the same framing
/// `GeneralDigest.finish` + `processLength` produce.
fn digest(msg: &[u8]) -> [u8; 32] {
    let mut padded = msg.to_vec();
    let bitlen = (msg.len() as u64) * 8;
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bitlen.to_be_bytes());

    let mut state = IV;
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 16];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        sha256_compress(&mut state, &w);
    }
    let mut out = [0u8; 32];
    for (i, s) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&s.to_be_bytes());
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// FIPS 180-4 published vector. A compression function that is subtly wrong
/// (a mis-transcribed rotate, a `>>` where the spec says `>>>`) still
/// produces stable, self-consistent output, so a round trip against itself
/// proves nothing — only a published vector does.
#[test]
fn sha256_matches_the_published_abc_vector() {
    assert_eq!(
        hex(&digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sha256_matches_the_published_empty_vector() {
    assert_eq!(
        hex(&digest(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// Two-block message: exercises the message schedule carrying across a
/// block boundary, which the one-block vectors above cannot reach.
#[test]
fn sha256_matches_the_published_two_block_vector() {
    assert_eq!(
        hex(&digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

/// A long message (1 000 000 'a's is the classic NIST case, shortened here
/// to keep the test fast while still spanning many blocks).
#[test]
fn sha256_matches_a_multi_block_vector() {
    let msg = vec![b'a'; 1000];
    assert_eq!(
        hex(&digest(&msg)),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

#[test]
fn md5_process_block_matches_rfc1321() {
    let mut padded = b"abc".to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&(24u64).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut x = [0u32; 16];
    for (i, word) in padded.chunks_exact(4).enumerate() {
        x[i] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
    }
    md5_process_block(&mut state, &mut x);
    assert_eq!(x, [0u32; 16]);
    let mut out = [0u8; 16];
    for (i, w) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    assert_eq!(
        out,
        [
            0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0,
            0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1, 0x7f, 0x72,
        ]
    );
}


fn digest512(msg: &[u8]) -> [u8; 64] {
    let mut padded = msg.to_vec();
    let bitlen = (msg.len() as u128) * 8;
    padded.push(0x80);
    while padded.len() % 128 != 112 {
        padded.push(0);
    }
    padded.extend_from_slice(&bitlen.to_be_bytes());

    let mut state: [u64; 8] = [
        0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
        0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
    ];
    for block in padded.chunks_exact(128) {
        let mut w = [0u64; 16];
        for (i, word) in block.chunks_exact(8).enumerate() {
            w[i] = u64::from_be_bytes(word.try_into().unwrap());
        }
        sha512_compress(&mut state, &w);
    }
    let mut out = [0u8; 64];
    for (i, s) in state.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&s.to_be_bytes());
    }
    out
}

#[test]
fn sha512_matches_published_vectors() {
    assert_eq!(
        hex(&digest512(b"abc")),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    assert_eq!(
        hex(&digest512(b"")),
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
}
